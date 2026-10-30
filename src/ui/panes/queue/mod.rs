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
pub(crate) use play::{hold_mpd_while_resolving, play_from_stop};
use play::play_queue_song;
#[allow(unused_imports)]
pub(crate) use play::resolved_stream_expired;
pub(crate) use play::next_track_resolving;
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
    /// Round 95: the Duration column's spinner goes through the shared frame
    /// clock (`Ctx::animator`), which is asked only while a stream resolve is
    /// in flight — so the animation costs no frames while no row spins.
    /// The audio list has scrolled to the pasted-playlist import's loading
    /// row (2026-10-24): the scroll happens once, on the frame the import
    /// starts, so a later redraw cannot yank a list the user is reading.
    import_loading_shown: bool,
    /// The Video list's half of [`QueuePane::import_loading_shown`].
    video_import_loading_shown: bool,
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
    /// A track change whose playing row was not in the pane's queue copy yet
    /// (a resolve's swap lands its refresh just after the status poll that
    /// raised the change). Replayed by the `QueueChanged` arm, which is the
    /// refresh that makes the row findable.
    pending_follow_song_id: Option<u32>,
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
    #[track_caller]
    pub(super) fn note_user_navigation(&mut self) {
        // Logged with the caller's identity: a suppressed jump is only
        // diagnosable if the navigation that suppressed it can be attributed
        // (movement key, click, wheel, scrollbar drag — or a path that should
        // not count at all).
        let at = std::time::Instant::now();
        // `#[track_caller]` so the log names the real source: without it the
        // location resolves to this line, which made the previous round's
        // log useless for attributing a suppression.
        log::debug!(
            caller:? = std::panic::Location::caller().to_string().as_str();
            "Queue navigation noted (suppresses the follow jump for 5 s)"
        );
        self.follow.note_user_navigation(at);
    }
    /// Follow the track that just started, if this is a new one.
    ///
    /// The playing row is looked up from `ctx.queue`. That lookup can fail:
    /// `SongChanged` is driven by a status poll, while a resolve's in-place
    /// swap reaches MPD over the client channel and its queue refresh lands
    /// **after** that poll (measured 1.8 ms later in the live log). In that
    /// window MPD reports the replacement's new song id while `ctx.queue` still
    /// holds the old row, so nothing matches. Dropping the jump there is what
    /// the user saw as "the selection doesn't seem to be moving to the new
    /// track after it resolves and starts playing" — and it was invisible,
    /// because the whole follow block sat behind the failed lookup.
    ///
    /// A row that cannot be resolved yet is therefore **remembered** and
    /// replayed by the `QueueChanged` arm, once the refresh makes it findable.
    ///
    /// **The deferral is a decision too (2026-10-20).** "Not findable" is *not*
    /// the same thing as "a new track we should follow": `find_current_song_in_queue`
    /// returns `None` for a stopped player, and the row-resolve flow stops MPD
    /// on purpose ("Holding MPD while the requested row resolves") while
    /// `status.songid` still names the **previous** track. Arming a deferral
    /// from that id meant the later `QueueChanged` replayed a jump to the row
    /// that had just stopped, so starting an unresolved stream snapped the
    /// selection back to the previous item instead of leaving it on the row
    /// that was resolving and then playing (reported 2026-10-20).
    ///
    /// So the same three conditions decide here as everywhere else — a *new*
    /// track, the option on, and the gate not suppressed by the user's own
    /// navigation — and only a decided jump is remembered. A declined one
    /// clears the pending id: it is not owed to anyone.
    fn follow_current_song(&mut self, ctx: &Ctx, is_visible: bool) -> Result<()> {
        let found = ctx.find_current_song_in_queue().map(|(_, song)| song.id);
        let Some(song_id) = found.or(ctx.status.songid) else {
            // No song at all: nothing to follow and nothing to defer to.
            self.pending_follow_song_id = None;
            return Ok(());
        };
        // One decision per track change: a repeated SongChanged for the same
        // track (a re-render, a resume, a second status update) must not re-run
        // the jump and yank a selection the user has since moved.
        let new_track = self.followed_song_id != Some(song_id);
        if new_track {
            self.followed_song_id = Some(song_id);
        }
        let follow_enabled = ctx.config.select_current_song_on_change;
        let decided =
            new_track && follow_enabled && self.follow.should_follow(std::time::Instant::now());
        if new_track {
            log::debug!(
                song_id, enabled = follow_enabled, follows = decided,
                visible = is_visible, found = found.is_some();
                "The queue selection's follow decision for this track change"
            );
        }
        if found.is_none() {
            // The row is not in the queue copy yet (a swap's refresh has not
            // landed). Remember the jump only if one was actually decided.
            if decided {
                log::debug!(
                    song_id;
                    "The playing row is not in the queue copy yet; deferring the follow jump"
                );
                self.pending_follow_song_id = Some(song_id);
            } else {
                self.pending_follow_song_id = None;
            }
            return Ok(());
        }
        // A newer track change supersedes any deferral still waiting for a row
        // that never turned up.
        self.pending_follow_song_id = None;
        if !decided {
            return Ok(());
        }
        // The index from `ctx.queue` is deliberately not used: the pane's list
        // is filtered, so the row is located by id inside `place_follow`.
        self.place_follow(song_id, is_visible, ctx)
    }
    /// Replay a jump that was deferred because the row was not in the queue
    /// copy yet.
    ///
    /// The decision was already made when the change arrived, so the gate is
    /// **not** consulted again: a deferral is armed because the row was not
    /// findable, not because the user was navigating, and re-checking seconds
    /// later lets navigation that happened *after* the change discard a jump
    /// that was already owed. That is the reported "an unwarmed ▶▶ never causes
    /// the selection to update when the track actually starts playing" — the
    /// resolve takes seconds, so the replay routinely landed after the user had
    /// touched the queue.
    fn follow_song(&mut self, ctx: &Ctx, song_id: u32) {
        self.followed_song_id = Some(song_id);
        if !ctx.config.select_current_song_on_change {
            return;
        }
        if let Err(err) = self.place_follow(song_id, true, ctx) {
            log::error!(error:? = err; "Failed to place the deferred follow jump");
        }
    }
    /// Put the selection on the row with `song_id` at the top of the visible
    /// list (or centered, when the config asks for that), and repaint.
    ///
    /// The row is found **by id in the pane's own list**, not by an index from
    /// `ctx.queue`: the pane shows a *filtered* copy (`local_queue`, which drops
    /// hidden stream rows and the temp-play row), so the two lists can be offset
    /// and a raw index would land on the wrong row — or be clamped away
    /// entirely, which is the reported "the highlight/selection never moves to
    /// the top of the queue".
    fn place_follow(&mut self, song_id: u32, is_visible: bool, ctx: &Ctx) -> Result<()> {
        let Some(idx) = self.queue.items.iter().position(|song| song.id == song_id) else {
            // The row is not in the filtered list (it is hidden, or the swap's
            // refresh has not landed yet). Re-apply by id once it is.
            log::debug!(
                song_id;
                "The followed row is not in the pane's list yet; re-applying on the refresh"
            );
            self.pending_follow_song_id = Some(song_id);
            return Ok(());
        };
        match (is_visible, ctx.config.center_current_song_on_change) {
            // The default: land the new track at the **top** of the visible
            // list, as close as the viewport allows — watching the queue then
            // shows the playing track with the upcoming rows under it.
            (true, false) => self.queue.select_at_top(idx),
            (false, false) => {
                self.queue.select_at_top(idx);
                // The pane is off screen, so the viewport height is stale;
                // re-apply on the next show.
                self.pending_follow_to_top = Some(idx);
            }
            // The older centering placement, kept for anyone who prefers it.
            (true, true) => self.queue.select_idx(idx, usize::MAX),
            (false, true) => {
                self.queue.select_idx(idx, usize::MAX);
                self.should_center_cursor_on_current = true;
            }
        }
        // `ctx.render` reports a channel error rather than an `anyhow::Error`;
        // the inline code this replaced used `?` on it, which converted via
        // `From`.
        ctx.render().map_err(anyhow::Error::from)
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
    #[cfg(test)]
    pub(super) fn local_queue_for_test(ctx: &Ctx) -> Vec<Song> {
        Self::local_queue(ctx)
    }

    /// Whether a queue row is shown by the Audio list.
    ///
    /// The single definition of "visible row", shared by the list and by the
    /// box title's `N songs` count. They were computed from different sources
    /// (the list filtered, the title did not), which is how a queue of 98
    /// buffered streams could read "98 songs" above an empty list — round 111.
    pub(crate) fn row_is_visible(ctx: &Ctx, song: &Song) -> bool {
        // Round 91: a pasted stream link is queue content from the moment it
        // is queued, before any stream exists - the yt-info cache only knows
        // it once it has been played.
        let tagged_link = crate::shared::ytdlp::tagged_stream_entry(&song.file).is_some();
        // Round 111: a buffered stream URL (`/s2u/<class>/<key>`) is this
        // app's own proxy URL for a queue row, so it is queue content by
        // construction and must never be treated as a hidden radio stream.
        // Missing this made a restored queue of web streams render as an
        // **empty list while the count line still read "N songs"**: every row
        // was a `http://` URL (so `is_stream_url` said yes), carried no
        // `#s2u-audio` tag, and — after a restart — was not yet in the
        // in-memory yt-info map.
        let buffered = crate::core::stream_cache::is_buffered_url(&song.file);
        let hidden_stream = crate::ui::panes::radio::is_stream_url(&song.file)
            && !buffered
            && !tagged_link
            && !ctx.yt_info.borrow().contains_key(&song.file);
        !hidden_stream && Some(song.id) != ctx.temp_play_id.get()
    }

    fn local_queue(ctx: &Ctx) -> Vec<Song> {
        ctx.queue
            .iter()
            .filter(|song| Self::row_is_visible(ctx, song))
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
            import_loading_shown: false,
            video_import_loading_shown: false,
            warmed_selection_id: None,
            follow: follow::FollowGate::default(),
            followed_song_id: None,
            pending_follow_to_top: None,
            pending_follow_song_id: None,
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
                let was_stale = self.queue.items.len() != ctx.queue.len();
                let before = self.queue.items.len();
                self.queue.items.clone_from(&Self::local_queue(ctx));
                // The list's measured length is set by `before_show`, i.e. at
                // the *previous* render, so it still describes the list as it
                // was. Anything that selects by index here would be clamped
                // against the stale count (a 3-row list receiving a 5-row
                // import clamps index 3 to 2), so the length is updated first.
                self.queue
                    .state
                    .set_content_len(Some(self.queue.items.len()));
                // A playlist import appends rows; the highlight belongs on the
                // first row it added. The refresh is what makes that row
                // findable, so the jump is applied here — and only when the
                // list actually grew, because a resolve's swap replaces a row
                // without changing the length and must not move the selection.
                if let Some(want) = ctx.pending_select_on_growth.take() {
                    // The row is located by **what was not there before**, not by
                    // a raw index: the pane shows a filtered copy of the queue
                    // (`local_queue` drops hidden stream rows and the temp-play
                    // row), so `ctx.queue` indices and pane indices differ. `want`
                    // is the first-added row's position in the *queue*; matching
                    // it against the newly-appeared ids keeps the two in step.
                    let grew = self.queue.items.len() > before;
                    if grew {
                        let first_new = self.queue.items[before.min(self.queue.items.len())..]
                            .first()
                            .map(|song| song.id);
                        let idx = first_new
                            .and_then(|id| self.queue.items.iter().position(|s| s.id == id))
                            .or(Some(want.min(self.queue.items.len().saturating_sub(1))));
                        if let Some(idx) = idx {
                            log::debug!(
                                idx, added = self.queue.items.len() - before;
                                "Selecting the first row the queue grew by"
                            );
                            self.queue.select_at_top(idx);
                            if !is_visible {
                                self.pending_follow_to_top = Some(idx);
                            }
                            ctx.render().map_err(anyhow::Error::from)?;
                        }
                    }
                }
                // 2026-10-15: a `SongChanged` that arrived while the pane's
                // queue copy was stale could not resolve the playing row, so
                // its follow jump was dropped. Now that the refreshed queue is
                // in, replay it — the row is findable again.
                if let Some(song_id) = self.pending_follow_song_id.take() {
                    log::debug!(
                        song_id, stale = was_stale;
                        "Replaying the follow jump now that the queue refresh landed"
                    );
                    self.follow_song(ctx, song_id);
                }
            }
            UiEvent::SongChanged => {
                // A track without markers replaced a chaptered one: the
                // Chapters view falls back to the audio list.
                self.follow_mpd_chapters(ctx);
                self.follow_current_song(ctx, is_visible)?;
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod follow_the_track_tests {
    use super::{QueuePane, follow};
    use crate::mpd::commands::{Song, State};
    use crate::ui::panes::Pane;

    fn row(id: u32, file: impl Into<String>) -> Song {
        Song { id, file: file.into(), ..Default::default() }
    }

    /// A pane over a 30-row queue with `playing` as the current song, plus the
    /// id of that row. The viewport is 10 rows so "at the top" is observable.
    struct Rig {
        ctx: crate::ctx::Ctx,
        pane: QueuePane,
    }

    fn rig(playing_idx: usize, len: usize) -> Rig {
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        // The fixture takes the receivers; leak clones so the senders the code
        // under test uses stay connected.
        Box::leak(Box::new(app.1.clone()));
        Box::leak(Box::new(work.1.clone()));
        Box::leak(Box::new(client.1.clone()));
        let mut ctx = crate::tests::fixtures::ctx(
            (app.0, app.1),
            (work.0, work.1),
            (client.0, client.1),
        );
        ctx.queue = (0..len).map(|i| row(i as u32 + 1, format!("/music/{i}.flac"))).collect();
        ctx.status.state = State::Play;
        ctx.status.song = Some(playing_idx);
        ctx.status.songid = ctx.queue.get(playing_idx).map(|s| s.id);
        let mut pane = QueuePane::new(&ctx);
        // A real viewport, so `select_at_top` clamps against it.
        pane.areas[super::Areas::Table] = ratatui::layout::Rect::new(0, 0, 80, 10);
        pane.queue.state.set_content_and_viewport_len(pane.queue.len(), 10);
        // Skip the first-show jump so the tests exercise the SongChanged path.
        pane.startup_jump_done = true;
        Rig { ctx, pane }
    }

    impl Rig {
        /// Drive a real `SongChanged` through the pane, as the event loop does.
        fn song_changed(&mut self) {
            let mut event = crate::ui::UiEvent::SongChanged;
            self.pane.on_event(&mut event, true, &self.ctx).expect("the pane handles it");
        }

        /// Move on to `idx`: set the status, then fire the event.
        /// Move playback to `idx`, keeping the pane's list in step with
        /// `ctx.queue` the way the real `QueueChanged` refresh does (the pane
        /// shows a filtered copy, so the two must not drift apart silently).
        fn play(&mut self, idx: usize) {
            self.ctx.status.song = Some(idx);
            self.ctx.status.songid = self.ctx.queue.get(idx).map(|s| s.id);
            self.pane.queue.items.clone_from(&QueuePane::local_queue(&self.ctx));
            self.song_changed();
        }

        fn selected(&self) -> Option<usize> {
            self.pane.queue.selected_with_idx().map(|(idx, _)| idx)
        }

        fn offset(&self) -> usize {
            self.pane.queue.state.offset()
        }
    }

    /// The default behaviour: the selection follows the new track and lands at
    /// the **top** of the visible list.
    #[test]
    fn the_selection_follows_to_the_top() {
        let mut r = rig(0, 30);
        r.play(12);
        assert_eq!(r.selected(), Some(12), "the selection moved to the new track");
        assert_eq!(r.offset(), 12, "and it is the first visible row");
    }

    /// A track near the end still lands at the top, with the offset clamped so
    /// the list never scrolls past its end.
    #[test]
    fn a_track_near_the_end_lands_at_the_top_with_the_offset_clamped() {
        let mut r = rig(0, 30);
        r.play(29);
        assert_eq!(r.selected(), Some(29));
        // 30 rows, 10 visible: the last possible offset is 20.
        assert_eq!(r.offset(), 20, "the offset clamps to len - viewport");
    }

    /// Scrolling or moving the selection within the 5 s window stands the jump
    /// down: the user's position is left exactly where they put it.
    #[test]
    fn recent_navigation_suppresses_the_jump() {
        let mut r = rig(0, 30);
        // The user moves to row 3 by hand...
        r.pane.queue.select_idx(3, 0);
        r.pane.note_user_navigation();
        // ...and the track changes immediately after.
        r.play(20);
        assert_eq!(r.selected(), Some(3), "the user's selection is left alone");
    }

    /// A suppressed jump does not re-apply for the same track, even after the
    /// window has elapsed: the user's position survives the whole track.
    #[test]
    fn a_suppressed_jump_does_not_re_apply_for_the_same_track() {
        let mut r = rig(0, 30);
        r.pane.queue.select_idx(5, 0);
        r.pane.note_user_navigation();
        r.play(18);
        assert_eq!(r.selected(), Some(5));
        // A repeated SongChanged for the same song (a re-render, a resume).
        r.song_changed();
        assert_eq!(r.selected(), Some(5), "the decision is made once per track");
    }

    /// The gate itself: a second track change inside the window is also
    /// suppressed (the window is measured from the user's navigation).
    #[test]
    fn a_second_change_inside_the_window_is_also_suppressed() {
        let mut gate = follow::FollowGate::default();
        gate.note_user_navigation(std::time::Instant::now());
        assert!(!gate.should_follow(std::time::Instant::now()), "the first change stands down");
        assert!(
            !gate.should_follow(std::time::Instant::now()),
            "still inside the window, so the next change stands down too"
        );
    }

    /// With the user never navigating, successive tracks are all followed.
    #[test]
    fn successive_tracks_are_followed_when_the_user_never_navigates() {
        let mut r = rig(0, 30);
        r.play(4);
        assert_eq!(r.selected(), Some(4));
        r.play(9);
        assert_eq!(r.selected(), Some(9), "the next track is followed again");
    }

    /// The option switches the whole behaviour off: the selection stays put.
    #[test]
    fn the_option_off_leaves_the_selection_alone() {
        let mut r = rig(0, 30);
        // `select_current_song_on_change` is on by default in this fork.
        assert!(r.ctx.config.select_current_song_on_change);
        r.pane.queue.select_idx(2, 0);
        // Replace the config with the option off.
        let mut config = (*r.ctx.config).clone();
        config.select_current_song_on_change = false;
        r.ctx.config = std::sync::Arc::new(config);
        r.play(15);
        assert_eq!(r.selected(), Some(2), "the selection is untouched");
    }

    /// The window boundary: exactly 5 s after navigation the jump proceeds.
    #[test]
    fn the_window_boundary_is_exclusive() {
        let mut gate = follow::FollowGate::default();
        let base = std::time::Instant::now();
        gate.note_user_navigation(base);
        assert!(
            gate.should_follow(base + follow::FOLLOW_SUPPRESS_WINDOW),
            "the window has elapsed"
        );
        let mut gate = follow::FollowGate::default();
        gate.note_user_navigation(base);
        assert!(
            !gate.should_follow(base + follow::FOLLOW_SUPPRESS_WINDOW
                - std::time::Duration::from_millis(1)),
            "one millisecond short is still suppressed"
        );
    }

    /// **The reported bug: "the selection doesn't seem to be moving to the
    /// new track after it resolves and starts playing".**
    ///
    /// The step resolves a row and the swap gives it a **new id**
    /// (`delete_id` + `add_id`). `SongChanged` therefore fires with an id the
    /// pane has never seen, which must still be followed — the id is not the
    /// track, and treating "a new id" as "the same track already handled"
    /// would leave the selection behind.
    #[test]
    fn a_row_whose_id_changed_after_a_swap_is_still_followed() {
        let mut r = rig(0, 30);
        // The look-ahead swapped the row that is about to play, so MPD now
        // reports a *different* id for it at the same position.
        let pos = 12;
        r.ctx.queue[pos].id = 999;
        r.play(pos);
        assert_eq!(
            r.selected(),
            Some(pos),
            "the selection follows the row even though its id changed"
        );
        assert_eq!(r.offset(), pos, "and it lands at the top of the list");
    }

    /// The same, with the id change happening *while* the same position keeps
    /// playing: a repeated event for the new id must not be treated as a
    /// second, different track.
    #[test]
    fn a_swap_then_a_repeat_for_the_new_id_keeps_the_selection_put() {
        let mut r = rig(0, 30);
        let pos = 8;
        r.ctx.queue[pos].id = 555;
        r.play(pos);
        assert_eq!(r.selected(), Some(pos));
        // The user moves away, then a repeated SongChanged arrives for the
        // same (new) id: it must not yank them again.
        r.pane.queue.select_idx(2, 0);
        r.song_changed();
        assert_eq!(r.selected(), Some(2), "the user's position is respected");
    }

    /// **The reported sequence: a step resolves, the swap replaces the row
    /// (new id, new file), and the selection must land on the row that is
    /// playing.**
    ///
    /// `SongChanged` may arrive before the pane has rebuilt its list from the
    /// refreshed queue (the swap reaches MPD over the client channel, and the
    /// `QueueChanged` that follows carries the new row). The jump must survive
    /// that ordering: it is computed from `ctx.queue`, and the pane's own copy
    /// is replaced afterwards.
    #[test]
    fn the_selection_lands_after_a_swap_even_if_the_pane_list_is_stale() {
        let mut r = rig(0, 30);
        // The look-ahead swaps the row that is about to play: MPD gets a new
        // id and the file becomes the buffered proxy URL.
        let pos = 20;
        r.ctx.queue[pos].id = 777;
        // The swap registers the buffered URL in `yt_info` as it replaces the
        // row (`apply_resolved_streams`), which is what keeps the row visible
        // in the pane's filtered list — a stream row whose URL is unknown is
        // hidden, so the jump could not find it.
        r.ctx.queue[pos].file = "http://127.0.0.1:39235/s2u/audio/newstream".to_owned();
        r.ctx.yt_info.borrow_mut().insert(
            r.ctx.queue[pos].file.clone(),
            crate::shared::ytdlp::YtStreamInfo {
                url: r.ctx.queue[pos].file.clone(),
                title: "Swapped".to_owned(),
                ..Default::default()
            },
        );
        // The pane still holds the pre-swap list (its `QueueChanged` has not
        // run yet), then the track change arrives: the jump is deferred.
        r.ctx.status.song = Some(pos);
        r.ctx.status.songid = Some(777);
        r.song_changed();
        assert_eq!(
            r.pane.pending_follow_song_id,
            Some(777),
            "the change is remembered while the pane's list is stale"
        );

        // The refresh carrying the swapped row lands.
        let mut ev = crate::ui::UiEvent::QueueChanged;
        r.pane.on_event(&mut ev, true, &r.ctx).expect("the pane handles it");
        assert_eq!(
            r.selected(),
            Some(pos),
            "the deferred jump lands once the refreshed list holds the row"
        );
        assert_eq!(r.offset(), pos, "at the top of the list");
    }

    /// **Reported 2026-10-20: starting an unresolved stream snapped the
    /// selection back to the previous item.**
    ///
    /// The row-resolve flow stops MPD on purpose ("Holding MPD while the
    /// requested row resolves") while `status.songid` still names the
    /// **previous** track. A stopped player makes `find_current_song_in_queue`
    /// return `None`, which the deferral read as "a new track whose row is not
    /// in the queue copy yet" — so it remembered the *previous* id, and the
    /// queue refresh replayed that jump. The live log shows it exactly: the
    /// deferral armed song `25` (the track that had just stopped) while the
    /// user was resolving row `81`, and the replay put the selection on `25`.
    ///
    /// A deferral is a decision now: no new track, no deferred jump.
    #[test]
    fn a_stop_while_a_row_resolves_does_not_replay_the_previous_track() {
        let mut r = rig(0, 30);
        // Row 0 (id 1) has been playing; its change was already decided.
        r.song_changed();
        assert_eq!(r.pane.followed_song_id, Some(1), "the previous track was handled");

        // The user starts row 12 (an unresolved link): the app holds MPD — a
        // stop — and the highlight stays on the row they started.
        r.pane.queue.select_idx(12, 0);
        r.ctx.status.state = State::Stop;
        r.ctx.status.songid = Some(1);
        r.song_changed();
        assert_eq!(
            r.pane.pending_follow_song_id, None,
            "a stopped player has nothing to follow, so the previous track's id is not deferred"
        );

        // The resolve lands and the swap's queue refresh arrives.
        let mut ev = crate::ui::UiEvent::QueueChanged;
        r.pane.on_event(&mut ev, true, &r.ctx).expect("the pane handles it");
        assert_eq!(
            r.selected(),
            Some(12),
            "the highlight stays on the row the user started, not the previous item"
        );
    }

    /// The replay deliberately does not re-consult the gate — so a deferral
    /// must never be armed from a decision the gate declined. Otherwise a
    /// navigation just before the change is honoured, and then seconds later
    /// the deferred replay yanks the selection anyway.
    #[test]
    fn a_suppressed_decision_does_not_arm_a_deferred_jump() {
        let mut r = rig(0, 30);
        r.pane.queue.select_idx(3, 0);
        r.pane.note_user_navigation();
        // A new track whose row the pane's stale list does not have yet.
        let pos = 20;
        r.ctx.queue[pos].id = 777;
        r.ctx.status.song = Some(pos);
        r.ctx.status.songid = Some(777);
        r.song_changed();
        assert_eq!(
            r.pane.pending_follow_song_id, None,
            "the gate declined this change, so nothing is owed to the refresh"
        );
        let mut ev = crate::ui::UiEvent::QueueChanged;
        r.pane.on_event(&mut ev, true, &r.ctx).expect("the pane handles it");
        assert_eq!(r.selected(), Some(3), "the user's position survives the refresh");
    }

    /// **The second reported miss.** The follow jump runs (the log shows
    /// `follows=true`), and then `QueueChanged` arrives ~2 ms later and
    /// rebuilds the pane's list — because the swap reached MPD over the client
    /// channel and its queue refresh lands after the status poll that carried
    /// the song change. The jump must survive that refresh: it is the row the
    /// user is watching.
    ///
    /// Reported as "the selection doesn't seem to be moving to the new track
    /// after it resolves and starts playing".
    #[test]
    fn the_jump_survives_the_queue_refresh_that_follows_it() {
        let mut r = rig(0, 60);
        r.play(30);
        assert_eq!(r.selected(), Some(30), "the jump ran");
        assert_eq!(r.offset(), 30, "and landed the row at the top");

        // The swap's queue refresh lands right after (a different content
        // length is what shifts indices if the selection is recomputed).
        let mut ev = crate::ui::UiEvent::QueueChanged;
        r.pane.on_event(&mut ev, true, &r.ctx).expect("handled");
        assert_eq!(
            r.selected(),
            Some(30),
            "the refresh must not move the selection off the playing row"
        );
        assert_eq!(r.offset(), 30, "nor scroll it away");
    }

    /// **The root cause.** `SongChanged` is driven by a status poll; a
    /// resolve's swap reaches MPD over the client channel and its queue
    /// refresh lands *after* that poll (measured 1.8 ms later). In that window
    /// MPD reports the replacement's new song id while `ctx.queue` still holds
    /// the old row, so the playing row cannot be resolved — and the old code
    /// dropped the jump on the floor, silently, because the whole follow block
    /// sat behind that lookup.
    ///
    /// The jump must be remembered and replayed by the refresh.
    #[test]
    fn a_change_whose_row_is_not_in_the_queue_yet_is_deferred_and_replayed() {
        let mut r = rig(0, 30);
        // MPD has moved to a row the queue copy does not contain yet.
        r.ctx.status.song = Some(20);
        r.ctx.status.songid = Some(9999);
        r.song_changed();
        assert_eq!(
            r.selected(),
            Some(0),
            "nothing can be selected while the row is unknown"
        );
        assert_eq!(
            r.pane.pending_follow_song_id,
            Some(9999),
            "the change is remembered instead of dropped"
        );

        // The swap's queue refresh lands and the row appears.
        r.ctx.queue[20].id = 9999;
        let mut ev = crate::ui::UiEvent::QueueChanged;
        r.pane.on_event(&mut ev, true, &r.ctx).expect("handled");

        assert_eq!(
            r.selected(),
            Some(20),
            "the deferred jump is replayed once the row is findable"
        );
        assert_eq!(r.offset(), 20, "and it lands at the top of the list");
        assert_eq!(r.pane.pending_follow_song_id, None, "the deferral is consumed");
    }

    /// A deferred jump for a row that never appears does not move the
    /// selection, and does not crash or loop: each refresh re-arms the
    /// deferral, and a later track change supersedes it.
    #[test]
    fn a_deferred_jump_for_a_row_that_never_appears_leaves_the_selection_alone() {
        let mut r = rig(0, 30);
        r.ctx.status.songid = Some(9999);
        r.ctx.status.song = Some(20);
        r.song_changed();
        assert_eq!(r.pane.pending_follow_song_id, Some(9999));
        // The refresh arrives without that row (it was consumed/removed).
        let mut ev = crate::ui::UiEvent::QueueChanged;
        r.pane.on_event(&mut ev, true, &r.ctx).expect("handled");
        assert_eq!(r.selected(), Some(0), "the selection is left alone");
        // A real track change afterwards takes over the pending follow.
        r.play(4);
        assert_eq!(r.selected(), Some(4), "the later change is followed normally");
        assert_eq!(r.pane.pending_follow_song_id, None, "and clears the deferral");
    }

    /// **The reported bug: "if ▶▶ needs a resolve … the song starts, but the
    /// highlight/selection never moves to the top of the queue".**
    ///
    /// `find_current_song_in_queue()` indexes **`ctx.queue`**, but the pane
    /// shows a **filtered** copy (`local_queue` drops hidden stream rows and
    /// the temp-play row). Applying a raw `ctx.queue` index to that shorter
    /// list lands on the wrong row, or is clamped away entirely. The jump must
    /// locate the row **by id** in the pane's own list.
    #[test]
    fn the_jump_finds_the_row_in_the_filtered_list_not_by_raw_index() {
        let mut r = rig(0, 30);
        // Two rows ahead of the target are hidden from the pane (they are
        // temporary stream entries), so the pane's list is offset by two.
        let target_pos = 12;
        let target_id = r.ctx.queue[target_pos].id;
        r.pane.queue.items.remove(0);
        r.pane.queue.items.remove(0);
        assert_eq!(r.pane.queue.items.len(), 28, "the pane list is shorter");

        r.ctx.status.song = Some(target_pos);
        r.ctx.status.songid = Some(target_id);
        r.song_changed();

        // The row must be selected where it actually sits in the pane: two
        // positions earlier than its index in ctx.queue.
        let selected = r.selected().expect("a row is selected");
        assert_eq!(
            r.pane.queue.items[selected].id, target_id,
            "the selection landed on the playing row, not an offset one"
        );
        assert_eq!(selected, target_pos - 2, "at its position in the pane's list");
        assert_eq!(r.offset(), target_pos - 2, "and at the top of the viewport");
    }

    /// The warmed case the user reports as working: the row is already there,
    /// and the jump lands it at the top. Covered too, so the two paths stay in
    /// step.
    #[test]
    fn the_warmed_case_lands_the_row_at_the_top() {
        let mut r = rig(0, 30);
        r.play(9);
        assert_eq!(r.selected(), Some(9));
        assert_eq!(r.offset(), 9);
    }

    /// **The reported bug: "an unwarmed ▶▶ never causes the selection to update
    /// when the track actually starts playing".**
    ///
    /// The resolve takes seconds, so the deferred replay lands well after the
    /// change was raised. Re-checking the navigation gate at replay time let a
    /// touch on the queue *after* the jump was owed discard it — even though the
    /// deferral was armed because the row was not findable, not because the user
    /// was navigating.
    #[test]
    fn navigation_after_a_deferral_does_not_discard_the_owed_jump() {
        let mut r = rig(0, 30);
        // MPD moved to a row the pane's list does not have yet: deferred.
        r.ctx.status.song = Some(20);
        r.ctx.status.songid = Some(9999);
        r.song_changed();
        assert_eq!(r.pane.pending_follow_song_id, Some(9999), "armed");

        // The user then touches the queue while the resolve is still running
        // (seconds later in real use).
        r.pane.note_user_navigation();

        // The refresh that carries the swapped row lands.
        r.ctx.queue[20].id = 9999;
        let mut ev = crate::ui::UiEvent::QueueChanged;
        r.pane.on_event(&mut ev, true, &r.ctx).expect("handled");

        assert_eq!(
            r.selected(),
            Some(20),
            "the jump was already owed; a later touch must not discard it"
        );
        assert_eq!(r.offset(), 20, "landed at the top of the list");
    }

    /// No current song is a no-op: nothing is selected or scrolled.
    #[test]
    fn no_current_song_is_a_no_op() {
        let mut r = rig(0, 30);
        r.pane.queue.select_idx(7, 0);
        r.ctx.status.song = None;
        r.ctx.status.songid = None;
        r.song_changed();
        assert_eq!(r.selected(), Some(7), "the selection is left where it was");
    }

    /// **The second half of the user's acceptance test: the selection lands on
    /// the row an unwarmed `▶▶` started, at the top of the list.**
    ///
    /// Playback starting is only half of it. The follow jump locates the playing
    /// row **by id in the pane's list**, and the look ahead's in-place swap
    /// (`delete_id` + `add_id`) gives that row a *new* id — so this is exactly
    /// where the swap can make the selection miss the track it just started.
    ///
    /// The ids here are the ones from the live log (`814` stepped to, swapped
    /// under a new id), and the event order is the event loop's: the resolve
    /// landings first, then `SongChanged`, then the swap's `QueueChanged`.
    #[test]
    fn the_selection_follows_the_row_an_unwarmed_step_started() {
        use crate::tests::fixtures::fake_mpd_server::FakeMpdServer;
        use crate::ui::modals::paste::{YtAction, apply_resolved_streams};
        use crate::shared::ytdlp::YtStreamInfo;

        const LINK: &str = "https://www.youtube.com/watch?v=bbbbbbbbbbb";

        // A queue deep enough that "the top" is observable: the step's target
        // sits at index 12, below the fold of a 10-row viewport.
        let mut rows: Vec<(u32, String)> = (1200..1220)
            .map(|id| {
                (id, format!("https://rr1.googlevideo.com/videoplayback?expire=7258118400&id={id}"))
            })
            .collect();
        rows[12] = (814, format!("{LINK}#s2u-audio"));

        // A real MPD server, so the swap and the step run the production code.
        let server = FakeMpdServer::start(rows.clone());
        let mut client = server.client();

        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client_channel = crossbeam::channel::unbounded();
        let client_rx = client_channel.1.clone();
        Box::leak(Box::new(app.1.clone()));
        Box::leak(Box::new(work.1.clone()));
        let mut ctx = crate::tests::fixtures::ctx(
            (app.0, app.1),
            (work.0, work.1),
            (client_channel.0, client_channel.1),
        );
        ctx.queue = rows
            .iter()
            .map(|(id, file)| Song { id: *id, file: file.clone(), ..Default::default() })
            .collect();
        ctx.status.state = State::Play;
        ctx.status.song = Some(0);
        ctx.status.songid = Some(1200);
        ctx.status.nextsongid = Some(814);
        ctx.status.random = true;
        // A real session has resolved every stream row it shows, so `local_queue`
        // (which hides an unknown stream URL to keep the pane honest about what
        // it can play) keeps them. Without this the pane filters the queue down
        // to the single row this test resolved, and "index 12" cannot exist.
        {
            let mut yt_info = ctx.yt_info.borrow_mut();
            for (_, file) in &rows {
                yt_info.insert(
                    file.clone(),
                    YtStreamInfo {
                        url: file.clone(),
                        title: "row".to_owned(),
                        ..Default::default()
                    },
                );
            }
        }

        let mut pane = QueuePane::new(&ctx);
        pane.areas[super::Areas::Table] = ratatui::layout::Rect::new(0, 0, 80, 10);
        pane.queue.state.set_content_and_viewport_len(pane.queue.len(), 10);
        pane.startup_jump_done = true;
        // The user had browsed away from the playing row before pressing `▶▶`.
        pane.queue.select_idx(0, 0);

        let info = |url: &str| YtStreamInfo {
            url: url.to_owned(),
            original_url: LINK.to_owned(),
            title: "A stream".to_owned(),
            ..Default::default()
        };
        let run_landings = |rx: &crossbeam::channel::Receiver<crate::shared::events::ClientRequest>,
                            client: &mut crate::mpd::client::Client<'_>| {
            while let Ok(request) = rx.try_recv() {
                if let crate::shared::events::ClientRequest::Command(command) = request {
                    let _ = (command.callback)(client);
                }
            }
        };
        let sync = |ctx: &mut crate::ctx::Ctx, client: &mut crate::mpd::client::Client<'_>| {
            use crate::mpd::mpd_client::MpdClient;
            ctx.queue = client.playlist_info().expect("playlistinfo").unwrap_or_default();
        };

        // T: the step resolves AND would play row 814.
        super::next_track_resolving(&ctx);
        assert_eq!(ctx.pending_next_step.get(), Some(814), "the step is armed");

        // T+1.3 s: the look ahead swaps the same row in place — warm-up only.
        apply_resolved_streams(
            &ctx,
            vec![info("https://rr1.googlevideo.com/videoplayback?expire=7258118400&id=la")],
            YtAction::ReplaceInPlace(814),
            Vec::new(),
        );
        run_landings(&client_rx, &mut client);
        sync(&mut ctx, &mut client);
        assert!(
            server.playing_file().is_none(),
            "the warm-up must not start playback"
        );

        // T+2.7 s: the step's own landing starts the track.
        apply_resolved_streams(
            &ctx,
            vec![info("https://rr1.googlevideo.com/videoplayback?expire=7258118400&id=step")],
            YtAction::ReplaceAndPlay(814),
            Vec::new(),
        );
        run_landings(&client_rx, &mut client);
        sync(&mut ctx, &mut client);

        let playing_id = server.state().playing.expect("the step started a row");
        ctx.status.songid = Some(playing_id);
        ctx.status.song = ctx.queue.iter().position(|song| song.id == playing_id);
        assert!(
            ctx.status.song.is_some(),
            "the row that started must be in the queue copy"
        );

        // The event loop's order: the status poll raises `SongChanged`, then the
        // swap's refresh raises `QueueChanged`.
        pane.queue.items.clone_from(&QueuePane::local_queue(&ctx));
        let mut song_changed = crate::ui::UiEvent::SongChanged;
        pane.on_event(&mut song_changed, true, &ctx).expect("SongChanged is handled");
        let mut queue_changed = crate::ui::UiEvent::QueueChanged;
        pane.on_event(&mut queue_changed, true, &ctx).expect("QueueChanged is handled");

        // The jump is "as close to the top of the visible list as the viewport
        // allows": 20 rows in a 10-row viewport, so the furthest the list can
        // scroll with row 12 on top is `len - viewport` = 10. The selection is
        // the assertion that matters — it is on the row the step started.
        assert_eq!(
            pane.queue.selected_with_idx().map(|(idx, _)| idx),
            Some(12),
            "the selection must land on the row the step started (index 12)"
        );
        assert_eq!(
            pane.queue.state.offset(),
            10,
            "and that row is visible at the top of the scrolled list"
        );
        assert_eq!(
            pane.queue.selected_with_idx().map(|(_, song)| song.id),
            Some(playing_id),
            "the highlighted row is the one MPD is playing"
        );
    }
    /// **Adding a playlist lands the highlight on the first row of what was
    /// added (2026-10-15).**
    ///
    /// A playlist import appends its rows to the queue. The pane refreshed its
    /// list but left the selection where it was, so the highlight stayed on
    /// whatever old index it happened to hold — which after an add reads as
    /// "the selection landed somewhere in the middle of the new tracks". The
    /// user's requirement: the highlight goes to the **first** added row.
    #[test]
    fn adding_a_playlist_puts_the_selection_on_the_first_added_row() {
        let mut r = rig(0, 3);
        // The user has moved the selection somewhere in the existing queue.
        r.pane.queue.select_idx(1, 0);
        assert_eq!(r.selected(), Some(1), "the starting selection");

        // A playlist lands: five link rows are appended.
        let added: Vec<_> = (0..5)
            .map(|i| row(200 + i, format!("https://www.youtube.com/watch?v=vid{i}#s2u-audio")))
            .collect();
        let first_added = added[0].id;
        // The import asks for the first added row before its enqueue lands, then
        // the queue grows and the refresh arrives.
        r.ctx.pending_select_on_growth.set(Some(r.ctx.queue.len()));
        r.ctx.queue.extend(added);
        let mut ev = crate::ui::UiEvent::QueueChanged;
        r.pane.on_event(&mut ev, true, &r.ctx).expect("QueueChanged is handled");

        let (idx, song) = r.pane.queue.selected_with_idx().expect("something is highlighted");
        assert_eq!(
            song.id, first_added,
            "the highlight must land on the first row the playlist added \
             (id {first_added}) — got index {idx}, id {}",
            song.id
        );
    }

    /// **A resolve's refresh must not move the selection.**
    ///
    /// The growth path exists for a playlist import, but the same `QueueChanged`
    /// fires constantly for a resolve's in-place swap — which replaces a row and
    /// leaves the length unchanged. Selecting there would yank the highlight out
    /// from under the user every time a stream resolved, so the length test is
    /// what keeps the two apart.
    #[test]
    fn a_resolve_refresh_that_does_not_grow_leaves_the_selection_alone() {
        let mut r = rig(0, 4);
        r.pane.queue.select_idx(2, 0);
        let before = r.selected();
        assert_eq!(before, Some(2), "the starting selection");

        // A swap: the same number of rows, one of them replaced.
        r.ctx.queue[1] = row(99, "http://127.0.0.1:39235/s2u/audio/swapped");
        r.ctx.pending_select_on_growth.set(Some(0));
        let mut ev = crate::ui::UiEvent::QueueChanged;
        r.pane.on_event(&mut ev, true, &r.ctx).expect("QueueChanged is handled");

        assert_eq!(
            r.selected(),
            before,
            "a refresh that does not grow the list must leave the selection where \
             the user put it"
        );
    }

}

/// Round 111: a queue of **buffered web streams** (the report's 98-item
/// YouTube playlist). These rows are the stream-buffer proxy URLs s2udio
/// itself handed to MPD, and they must behave as ordinary queue content:
/// visible in the list, and counted in the total time.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod buffered_stream_queue_tests {
    use super::QueuePane;
    use crate::mpd::commands::Song;

    /// A proxy URL of the shape MPD actually stores for a stream row.
    const PROXY: &str = "http://127.0.0.1:39235/s2u/audio/f6ae1edea05a880a";

    fn ctx_with_queue(files: Vec<String>) -> crate::ctx::Ctx {
        let app = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        Box::leak(Box::new(app.1.clone()));
        Box::leak(Box::new(work.1.clone()));
        Box::leak(Box::new(client.1.clone()));
        let mut ctx =
            crate::tests::fixtures::ctx((app.0, app.1), (work.0, work.1), (client.0, client.1));
        ctx.queue = files
            .into_iter()
            .enumerate()
            .map(|(i, file)| Song { id: i as u32 + 1, file, ..Default::default() })
            .collect();
        ctx
    }

    /// The bug: a restored queue of buffered streams rendered as an empty list
    /// while the count line still said "N songs". Every row was a `http://`
    /// URL, so `is_stream_url` matched, and none carried a `#s2u-audio` tag or
    /// had reached the in-memory yt-info map — so all of them were filtered out
    /// as "hidden radio streams".
    #[test]
    fn buffered_proxy_rows_are_visible_queue_content() {
        let ctx = ctx_with_queue(vec![PROXY.to_string(), PROXY.replace("f6ae", "aaaa")]);
        let visible = QueuePane::local_queue_for_test(&ctx);
        assert_eq!(
            visible.len(),
            2,
            "buffered proxy rows are this app's own queue content and must be listed"
        );
    }

    /// The converse, so the fix cannot be "show every http:// URL": a genuine
    /// remote stream that is not a tagged link and not resolved yet stays
    /// hidden — that is the radio/temp-entry behaviour this filter exists for.
    #[test]
    fn an_unrelated_remote_stream_stays_hidden() {
        let ctx = ctx_with_queue(vec!["http://stream.example.com/live.mp3".to_string()]);
        assert!(
            QueuePane::local_queue_for_test(&ctx).is_empty(),
            "a non-buffered remote URL is still filtered out"
        );
    }

    /// A pasted queue link (tagged `#s2u-audio`, not yet resolved) was already
    /// visible before this round and must stay visible.
    #[test]
    fn a_tagged_link_is_still_visible() {
        let ctx = ctx_with_queue(vec![
            "https://www.youtube.com/watch?v=abc123#s2u-audio".to_string()
        ]);
        assert_eq!(QueuePane::local_queue_for_test(&ctx).len(), 1);
    }

    /// The second bug: MPD reports no duration for a web-stream row, so the
    /// total summed to zero even though each row's Duration cell rendered a
    /// real value. Local files must be unaffected and nothing double-counted.
    #[test]
    fn the_total_counts_each_row_once() {
        let mut ctx = ctx_with_queue(vec!["/music/a.flac".to_string(), "/music/b.flac".to_string()]);
        ctx.queue[0].duration = Some(std::time::Duration::from_secs(100));
        ctx.queue[1].duration = Some(std::time::Duration::from_secs(50));
        assert_eq!(
            crate::ui::modals::paste::queue_total_time(&ctx),
            std::time::Duration::from_secs(150),
            "MPD-known durations are summed as before"
        );
    }

    /// The count line and the list must agree, because they are now the same
    /// predicate. This is the invariant the bug violated: the title said
    /// "98 songs" while the list showed none.
    #[test]
    fn the_box_count_matches_the_visible_rows() {
        let ctx = ctx_with_queue(vec![
            PROXY.to_string(),
            PROXY.replace("f6ae", "bbbb"),
            "http://stream.example.com/live.mp3".to_string(),
            "/music/local.flac".to_string(),
        ]);
        let listed = QueuePane::local_queue_for_test(&ctx).len();
        let counted = ctx
            .queue
            .iter()
            .filter(|song| QueuePane::row_is_visible(&ctx, song))
            .count();
        assert_eq!(counted, listed, "the box title's count is the list's row count");
        // 2 buffered proxy rows + 1 local file; the unrelated stream is hidden.
        assert_eq!(listed, 3);
    }

    /// The startup total must use the same helper as the update path, or a
    /// restored stream queue shows "0:00 total time" until some later queue
    /// event recomputes it. `Ctx::new` summed `Song.duration` only — the same
    /// bug in a second place (round 111).
    ///
    /// This pins the helper's behaviour on the exact input the startup path
    /// hands it: MPD-known durations are summed, and a row MPD cannot time is
    /// not silently dropped from a queue that has other timed rows.
    #[test]
    fn the_total_survives_a_mix_of_timed_and_untimed_rows() {
        let mut ctx = ctx_with_queue(vec![
            "/music/a.flac".to_string(),
            PROXY.to_string(),
            "/music/b.flac".to_string(),
        ]);
        ctx.queue[0].duration = Some(std::time::Duration::from_secs(60));
        ctx.queue[2].duration = Some(std::time::Duration::from_secs(90));
        assert_eq!(
            crate::ui::modals::paste::queue_total_time(&ctx),
            std::time::Duration::from_secs(150),
            "an untimed stream row must not suppress the timed rows' total"
        );
    }

    /// A row MPD cannot time (a stream) and that has no resolved metadata
    /// contributes zero rather than panicking — the honest answer before its
    /// resolve lands.
    #[test]
    fn an_untimed_unresolved_row_contributes_zero() {
        let ctx = ctx_with_queue(vec![PROXY.to_string()]);
        assert_eq!(crate::ui::modals::paste::queue_total_time(&ctx), std::time::Duration::ZERO);
    }
}

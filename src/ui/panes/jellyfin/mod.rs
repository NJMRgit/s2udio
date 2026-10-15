use std::{
    collections::{HashMap, HashSet},
    io::Write,
};
use anyhow::Result;
use ratatui::{
    Frame, layout::{Alignment, Constraint, Layout, Rect},
    prelude::IntoCrossterm, style::{Color, Modifier},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use super::Pane;
use crate::{
    MpdQueryResult, config::tabs::{PaneType, PaneTypeDiscriminants, TreeBrowserArgs},
    ctx::Ctx, jellyfin::{Jellyfin, JfItem},
    mpd::{commands::State, mpd_client::MpdClient},
    shared::mpd_client_ext::{Enqueue, MpdClientExt},
    config::keys::{CommonAction, DirectoriesActions, GlobalAction},
    shared::{
        events::WorkRequest, keys::ActionEvent,
        macros::{modal, status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{
        UiEvent, input::InputResultEvent,
        image::{
            Backend as _, block::Block as ImageBlock, facade::EncodeData, iterm2::Iterm2,
            kitty::Kitty, sixel::Sixel, ueberzug::{Layer, Ueberzug},
        },
        modals::menu::modal::MenuModal, tree_browser::{TreeBrowserCore, TreeRowView},
    },
};

/// Result ids of the work-thread Jellyfin fetches.
pub const JF_VIEWS: &str = "jellyfin_views";
pub const JF_FOLDER: &str = "jellyfin_folder";
pub const JF_ARTISTS: &str = "jellyfin_artists";
pub const JF_ALBUMS: &str = "jellyfin_albums";
pub const JF_SONGS: &str = "jellyfin_songs";
pub const JF_ITEM: &str = "jellyfin_item";
pub const JF_RESUME: &str = "jellyfin_resume";
pub const JF_IMAGE: &str = "jellyfin_image";
/// Item metadata + image for the MPRIS bridge (played stream title/art).
pub const JF_MPRIS: &str = "jellyfin_mpris";
pub const JF_CHAPTERS: &str = "jellyfin_chapters";
/// An episode's season as an mpv playlist (played starting at the episode).
pub const JF_SEASON_PLAY: &str = "jellyfin_season_play";
/// Server-side search results (round 60 B2, `SearchHints`).
pub const JF_SEARCH: &str = "jellyfin_search";
const JF_PLAY: &str = "jellyfin_play";
/// Round 92: the animated "loading" indicator on the mode-toggle row. Braille
/// spinner frames (one column wide) plus a label; the frame is picked from
/// wall-clock time, and `LOADING_FRAME_MS` is both the frame step and the
/// interval of the redraw tick that keeps the spinner moving.
const LOADING_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOADING_LABEL: &str = "Loading…";
const LOADING_FRAME_MS: u64 = 120;
/// A tracked fetch that never reports back (dropped result, hung work
/// thread) must not leave the indicator spinning forever.
const LOADING_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(30);
/// Round 64: the small poster shelf between the items list and the Info
/// box (user feedback: the poster/preview must not dominate the Info box;
/// show a compact preview in the gap instead of a 40%-column art block).
/// The shelf is carved out of the items list's bottom when the terminal
/// has room for it. Round 65: shelf height doubled (9 -> 18) per user
/// feedback (poster/preview twice as tall). Round 66: the shelf WIDTH
/// follows the artwork — the image always displays at the full inner
/// height (POSTER_SHELF_H - 2) and the box widens for wider images (stays
/// centered; a 1-char blank margin between the artwork and the box on both
/// sides, outside the 1-char outline). Round 67: the framing outline is
/// removed (it resized while the artwork loaded) — the image keeps its
/// size/position, borderless.
const POSTER_SHELF_H: u16 = 18;
/// Default shelf width while the artwork bytes are still loading (also the
/// min sensible width); once the image is known the shelf expands to the
/// image's own aspect at the fixed height.
const POSTER_SHELF_W: u16 = 26;
const POSTER_SHELF_RIGHT_MARGIN: u16 = 6;
/// The tab's mode (round 60 B2): the library browser, or server-side
/// search across every library. Startup default: Libraries; the search
/// state lives for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JellyfinTabMode {
    Libraries,
    Search,
}
/// A node of the left tree. The tree mirrors the server: views (libraries),
/// artists and albums of music libraries, plain folders elsewhere.
#[derive(Debug, Clone, PartialEq)]
pub enum JfNodeKind {
    View(JfItem),
    Artist(JfItem),
    Album(JfItem),
    Folder(JfItem),
}
impl JfNodeKind {
    fn id(&self) -> &str {
        match self {
            Self::View(item)
            | Self::Artist(item)
            | Self::Album(item)
            | Self::Folder(item) => &item.id,
        }
    }
    fn label(&self) -> String {
        match self {
            Self::View(item)
            | Self::Artist(item)
            | Self::Album(item)
            | Self::Folder(item) => item.name.clone(),
        }
    }
    fn item(&self) -> &JfItem {
        match self {
            Self::View(item)
            | Self::Artist(item)
            | Self::Album(item)
            | Self::Folder(item) => item,
        }
    }
    fn key(&self) -> String {
        let kind = match self {
            Self::View(_) => "view",
            Self::Artist(_) => "artist",
            Self::Album(_) => "album",
            Self::Folder(_) => "folder",
        };
        format!("{kind}:{}", self.id())
    }
}

#[derive(Debug, Clone)]
struct JfNode {
    kind: JfNodeKind,
    depth: u8,
    expandable: bool,
    expanded: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneFocus {
    Tree,
    Items,
}
#[derive(Debug)]
pub struct JellyfinPane {
    /// Server connection (base url + token), loaded from jellytui's config.
    server: Option<Jellyfin>,
    /// Library views of the server.
    views: Vec<JfItem>,
    /// Folder children keyed by parent item id.
    folders: HashMap<String, Vec<JfItem>>,
    /// Artists of a music view, keyed by view id.
    artists: HashMap<String, Vec<JfItem>>,
    /// Albums of an artist, keyed by artist id.
    albums: HashMap<String, Vec<JfItem>>,
    /// Songs of an album, keyed by album id.
    songs: HashMap<String, Vec<JfItem>>,
    /// Visible tree rows.
    tree: Vec<JfNode>,
    tree_list: ListState,
    /// Right-pane rows (children of the selected node).
    items: Vec<JfItem>,
    item_list: ListState,
    /// The node whose children are shown on the right.
    selected: Option<JfNodeKind>,
    /// Expanded node keys.
    expanded: HashSet<String>,
    /// Config-missing / fetch error notice.
    error: Option<String>,
    /// The pane the user last navigated in; Enter acts on that pane.
    focus: PaneFocus,
    /// Queue id of a song played via PlayFile (temp entry, removed on song
    /// change / stop — mirrors the Radio pane).
    temp_play_id: Option<u32>,
    /// Tree-browser layout args from the config (defaults = today's
    /// constants: 50-col minimum tree, hidden <= 120).
    tree_args: TreeBrowserArgs,
    initialized: bool,
    /// Round 92: library fetches in flight (views / children / search). The
    /// loading indicator on the toggle row is up while this is > 0.
    fetches_in_flight: u32,
    /// When the in-flight count last changed (watchdog: a fetch that never
    /// reports back must not leave the indicator up forever).
    fetch_activity_at: std::time::Instant,
    /// Scheduler id of the indicator's redraw tick (None = no tick running).
    loading_tick: Option<crate::shared::id::Id>,
    /// When the indicator came up (drives the spinner frame; wall clock, so
    /// no render keeps its own animation state).
    loading_since: Option<std::time::Instant>,
    tree_area: Rect,
    items_area: Rect,
    info_area: Rect,
    /// Poster / episode preview of the selected item (terminal-side
    /// overlay, best image protocol for the terminal).
    poster: poster::JfPoster,
    /// Whether a modal (settings panel, popup, ...) is open on top of the
    /// tab; the poster overlay is not drawn while one is up.
    is_modal_open: bool,
    /// Round 64: the small poster shelf carved out of the items list's
    /// bottom (between the list and the info box); `None` when the
    /// terminal is too short for the shelf (the poster is then hidden).
    /// The poster is drawn here as a small overlay instead of filling a
    /// 40% column of the Info box.
    poster_shelf: Option<Rect>,
    /// Scroll state of the info-box text (below the poster).
    info_state: ListState,
    /// Number of rows in the info text (for the scroll bounds).
    info_items_len: usize,
    /// Key of the item whose info is shown (reset the scroll on change).
    info_song_id: Option<String>,
    /// The full metadata (overview/credits, fetched like the queue tab's
    /// info box) of the currently selected item, when it has arrived.
    full_item: Option<JfItem>,
    /// When the current item's info was first shown (starts the title
    /// marquee's static pause; wall-clock so wheel scrolling never nudges
    /// the animation).
    info_song_shown_at: Option<std::time::Instant>,
    /// Area of the info text's scrollbar (for click/drag scrolling).
    info_scrollbar_area: Rect,
    /// Drag state of the info text's scrollbar (thumb follows the pointer).
    info_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag,
    /// Drag state of the items-list scrollbar (Round 48: press-and-hold on
    /// the thumb/track, then the thumb follows the pointer anywhere).
    item_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag,
    /// Round 60 (B2): the active mode (Libraries browser or Search).
    mode: JellyfinTabMode,
    /// Click areas of the toggle row's two labels (Libraries, Search).
    toggle_areas: [Rect; 2],
    /// Click area of the `↰ Back` button (round 60 B3).
    back_area: Rect,
    /// Buffer id of the search query input (session-lived).
    search_buffer: crate::ui::input::BufferId,
    /// Keyboard phase of the search mode: true = the input row is focused,
    /// false = the results list is focused. Round 73.2: starts false and is
    /// only set by the `S` jump — Shift+Tab / the toggle click no longer
    /// focus the input.
    search_input_focused: bool,
    /// Consecutive Left presses at the search bar (round 63.1): the first
    /// Left navigates the text cursor, the second consecutive Left exits
    /// the search exactly like Esc #2. Reset by any other input result.
    search_left_presses: u8,
    /// A search request is in flight (dedupe rapid typing).
    search_pending: bool,
    /// The query of the last sent request; a result that no longer
    /// matches the input fires another request (typed-ahead keystrokes
    /// are never lost).
    last_sent_query: String,
    /// The current server-side search results.
    search_results: Vec<JfItem>,
    /// List state (selection) of the search results.
    search_results_state: ListState,
    /// Area of the search results list (mouse math).
    search_results_area: Rect,
}

impl JellyfinPane {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            server: None,
            views: Vec::new(),
            folders: HashMap::new(),
            artists: HashMap::new(),
            albums: HashMap::new(),
            songs: HashMap::new(),
            tree: Vec::new(),
            tree_list: ListState::default(),
            items: Vec::new(),
            item_list: ListState::default(),
            selected: None,
            expanded: HashSet::new(),
            error: None,
            focus: PaneFocus::Tree,
            temp_play_id: None,
            tree_args: ctx.config.tree_browser_args(PaneTypeDiscriminants::Jellyfin),
            initialized: false,
            fetches_in_flight: 0,
            fetch_activity_at: std::time::Instant::now(),
            loading_tick: None,
            loading_since: None,
            tree_area: Rect::default(),
            items_area: Rect::default(),
            info_area: Rect::default(),
            poster: poster::JfPoster::new(ctx),
            is_modal_open: false,
            poster_shelf: None,
            info_state: ListState::default(),
            info_items_len: 0,
            info_song_id: None,
            full_item: None,
            info_song_shown_at: None,
            info_scrollbar_area: Rect::default(),
            info_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            item_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            mode: JellyfinTabMode::Libraries,
            toggle_areas: [Rect::default(); 2],
            back_area: Rect::default(),
            search_buffer: crate::ui::input::BufferId::new(),
            // Round 73.2: the search page opens unfocused (Shift+Tab no
            // longer grabs the input; `S` does).
            search_input_focused: false,
            search_left_presses: 0,
            search_pending: false,
            last_sent_query: String::new(),
            search_results: Vec::new(),
            search_results_state: ListState::default(),
            search_results_area: Rect::default(),
        }
    }
}

mod browser;
mod context_menus;
mod info;
mod loading;
mod play;
mod poster;
mod render;
mod search;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

impl Pane for JellyfinPane {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &Ctx,
    ) -> Result<()> {
        JellyfinPane::render(self, frame, area, ctx)
    }
    /// The mode toggle row (`⭘ Libraries ● Search`) + the `↰ Back` button.
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        // Round 92: `first_show` = the tab has never been opened (or the
        // server was dropped after a config change). The cached views are
        // loaded before the fetch so the first frame is already populated;
        // the fetch itself still runs in the background (stale-while-
        // revalidate) and rewrites the cache.
        let first_show = !self.initialized;
        if first_show {
            self.load_cached_views(ctx);
        }
        if first_show || self.error.is_some() {
            self.fetch_views(ctx);
        }
        self.initialized = true;
        self.poster.redraw_next_render(ctx);
        // Round 60b (D4): returning to a tab left in search mode with the
        // input focused re-attaches the search buffer (on_hide released
        // it while the tab was away). Round 73.2: the flag is only set by
        // the `S` jump, so this re-attaches the caret the user asked for
        // and nothing else.
        if self.mode == JellyfinTabMode::Search && self.search_input_focused {
            ctx.input.insert_mode(self.search_buffer);
        }
        Ok(())
    }
    /// Round 63.1 (3): any release ends an armed scrollbar grab (the
    /// routed release may have landed on another pane).
    fn on_global_mouse_release(&mut self, _ctx: &Ctx) -> Result<()> {
        self.info_scrollbar_drag.disarm();
        self.item_scrollbar_drag.disarm();
        Ok(())
    }
    fn on_hide(&mut self, ctx: &Ctx) -> Result<()> {
        self.poster.hide(ctx);
        // Round 60b (D4): leaving the tab must drop the search input's
        // insert mode — the input manager is global and an unattended
        // insert buffer would swallow keys meant for the other tab.
        if ctx.input.is_active(self.search_buffer) {
            ctx.input.normal_mode();
        }
        Ok(())
    }
    fn on_event(
        &mut self,
        event: &mut UiEvent,
        is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        if self.handle_tree_events(event, is_visible, ctx)? {
            return Ok(());
        }
        match event {
            UiEvent::ModalOpened => {
                self.is_modal_open = true;
                self.poster.hide(ctx);
            }
            UiEvent::ModalClosed => {
                self.is_modal_open = false;
                self.poster.drawn_area = None;
                ctx.render()?;
            }
            UiEvent::Displayed => {
                self.poster.drawn_area = None;
                ctx.render()?;
            }
            UiEvent::Hidden if !is_visible => {
                self.poster.hide(ctx);
            }
            // Round 51: a Settings sign-in may have written the jellyfin
            // sidecar while the app runs (and the notice may be stale) —
            // drop the cached server so credentials reload and the views
            // are re-requested in the same session: immediately when the
            // tab is on screen, on the next show otherwise.
            UiEvent::ConfigChanged => {
                self.server = None;
                self.error = None;
                if is_visible {
                    self.fetch_views(ctx);
                } else {
                    self.initialized = false;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let position = event.into();
        if matches!(
            event.kind, MouseEventKind::LeftClick | MouseEventKind::DoubleClick
        ) {
            // The mode toggle row (Libraries | Search).
            for (idx, area) in self.toggle_areas.iter().enumerate() {
                if area.contains(position) {
                    let mode = if idx == 0 {
                        JellyfinTabMode::Libraries
                    } else {
                        JellyfinTabMode::Search
                    };
                    if self.mode != mode {
                        self.mode = mode;
                        self.poster.hide(ctx);
                        ctx.render()?;
                    }
                    return Ok(());
                }
            }
            // Round 60 (B3): the `↰ Back` button.
            if self.back_area.width > 0 && self.back_area.contains(position) {
                return self.select_parent(ctx);
            }
        }
        if self.mode == JellyfinTabMode::Search {
            return self.handle_search_mouse(event, ctx);
        }
        if self.tree_area.contains(event.into()) {
            return self.handle_tree_mouse(event, ctx);
        }
        if self.info_scrollbar_area.height > 0
            && matches!(
                event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. }
            ) && (self.info_scrollbar_area.contains(event.into())
                || (matches!(event.kind, MouseEventKind::Drag { .. })
                    && self.info_scrollbar_drag.is_active()))
        {
            let max = self.info_items_len.saturating_sub(self.info_area.height as usize);
            if max > 0 {
                let viewport_len = self.info_area.height as usize;
                let position = self.info_state.offset();
                let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
                if let Some(perc) = self
                    .info_scrollbar_drag
                    .handle(
                        event,
                        self.info_scrollbar_area,
                        max + 1,
                        viewport_len,
                        position,
                        begin_len,
                        end_len,
                    )
                {
                    let new = ((perc.clamp(0.0, 1.0)) * max as f64).floor() as usize;
                    if new != self.info_state.offset() {
                        *self.info_state.offset_mut() = new;
                        ctx.render()?;
                    }
                    return Ok(());
                }
            }
            return Ok(());
        }
        if self.info_area.contains(event.into()) && self.info_area.height > 0 {
            let dir = match event.kind {
                MouseEventKind::ScrollUp => -1,
                MouseEventKind::ScrollDown => 1,
                _ => return Ok(()),
            };
            let max = self.info_items_len.saturating_sub(self.info_area.height as usize)
                as i64;
            let new = (self.info_state.offset() as i64 + dir).clamp(0, max.max(0))
                as usize;
            if new != self.info_state.offset() {
                *self.info_state.offset_mut() = new;
                ctx.render()?;
            }
            return Ok(());
        }
        if self.items_area.contains(event.into()) {
            return self.handle_items_mouse(event, ctx);
        }
        Ok(())
    }
    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_global() {
            // Round 60 (B2): Shift+Tab toggles the mode while this tab is
            // focused (same claim as the playlists/MPD toggles).
            if matches!(action, GlobalAction::ToggleMpdMode) {
                return self.toggle_mode(ctx);
            }
            // Round 73.2 (revision B: `S`): jump to the search page with the
            // input focused. Claimed here (before the mode dispatch), so it
            // works from both the Libraries browser and the search page.
            if matches!(action, GlobalAction::LibrarySearch) {
                return self.jump_to_search(ctx);
            }
            event.abandon();
        }
        if self.mode == JellyfinTabMode::Search {
            return self.handle_search_action(event, ctx);
        }
        self.handle_tree_action(event, ctx)?;
        Ok(())
    }
    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &mut Ctx) -> Result<()> {
        if self.mode == JellyfinTabMode::Search {
            match kind {
                InputResultEvent::Push | InputResultEvent::Pop => {
                    self.search_left_presses = 0;
                    self.run_search(ctx);
                }
                InputResultEvent::Confirm => {
                    // Enter inside the input row moves the focus to the
                    // results: the shared input manager drops insert mode
                    // right after this handler, so an input that stays
                    // "focused" would render its cursor but never receive
                    // another keystroke (round 60b D4).
                    self.search_left_presses = 0;
                    self.release_search_input(ctx);
                }
                InputResultEvent::Cancel => {
                    // Round 62.1 (S2): Esc inside the search bar leaves the
                    // search page back to the Jellyfin browser (clear query
                    // + results).
                    self.exit_search(ctx)?;
                }
                InputResultEvent::AtStart => {
                    // Round 63.1 (2): Left with the cursor already at the
                    // input's start (empty query, or the cursor reached
                    // position 0) is the exit press — the buffer no longer
                    // swallows it as a text-cursor move.
                    self.exit_search(ctx)?;
                }
                InputResultEvent::CursorLeft => {
                    // Round 63.1 (2): the first Left at the bar navigates
                    // the text cursor; the SECOND consecutive Left exits
                    // exactly like Esc #2 (Left #2 parity with MPD).
                    if self.search_left_presses > 0 {
                        self.search_left_presses = 0;
                        self.exit_search(ctx)?;
                    } else {
                        self.search_left_presses = 1;
                    }
                }
                InputResultEvent::NoChange => {
                    self.search_left_presses = 0;
                }
            }
            ctx.render()?;
            return Ok(());
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
        JellyfinPane::on_query_finished(self, id, data, _is_visible, ctx)
    }
}

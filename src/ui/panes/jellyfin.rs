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
/// One search-result row: the item name (bold-ish white) with a dim
/// sublabel of its type + artist/album/series, so the match kind is
/// clear at a glance.
fn search_result_rows(items: &[JfItem]) -> Vec<ListItem<'static>> {
    items
        .iter()
        .map(|item| {
            let mut parts: Vec<String> = Vec::new();
            parts.push(match item.kind.as_str() {
                "Audio" => "Song".to_owned(),
                "MusicAlbum" => "Album".to_owned(),
                "MusicArtist" => "Artist".to_owned(),
                "Movie" => "Movie".to_owned(),
                "Episode" => "Episode".to_owned(),
                "Series" => "Series".to_owned(),
                other => other.to_owned(),
            });
            match item.kind.as_str() {
                "Audio" => {
                    if let Some(artist) = &item.artist {
                        parts.push(artist.clone());
                    }
                    if let Some(album) = &item.album {
                        parts.push(album.clone());
                    }
                }
                "Episode" => {
                    if let Some(series) = &item.series_name {
                        parts.push(series.clone());
                    }
                }
                _ => {}
            }
            let mut line = Line::from(Span::styled(
                item.name.clone(),
                ratatui::style::Style::default().fg(ratatui::style::Color::White),
            ));
            line.push_span(Span::styled(
                format!("  — {}", parts.join(" · ")),
                ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::DIM),
            ));
            ListItem::from(line)
        })
        .collect()
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
    poster: JfPoster,
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
            poster: JfPoster::new(ctx),
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
    /// Load the server credentials (cheap file read; done on first show and
    /// whenever a fetch failed for lack of config).
    fn load_server(&mut self, ctx: &Ctx) -> Option<&Jellyfin> {
        if self.server.is_none() {
            let path = ctx.config.jellyfin.config_file.clone();
            let path_str = path.to_string_lossy().into_owned();
            let expanded = crate::config::utils::tilde_expand(&path_str);
            // Round 51: s2udio's own Settings sidecar first
            // (`~/.config/s2udio/jellyfin.ron`, legacy `~/.config/rmpc/…`
            // honored), jellytui's config file is only an optional reuse
            // fallback — same ordering as the playback/MPRIS call sites
            // (`src/core/work.rs` jellyfin_handle).
            let sidecar = crate::config::jellyfin::jellyfin_sidecar_path();
            if let Some(server) =
                Jellyfin::load(std::path::Path::new(expanded.as_ref()), Some(&sidecar))
            {
                self.server = Some(server);
                // A successful load must clear the cached notice; the pane
                // re-attempts the load whenever the error is set.
                self.error = None;
            } else {
                self.error = Some(
                    "Jellyfin is not configured — press Esc → Settings → Jellyfin, \
                     enter Server URL / Username / Password and Sign in."
                        .to_owned(),
                );
            }
        }
        self.server.as_ref()
    }
    fn fetch_views(&mut self, ctx: &Ctx) {
        if self.load_server(ctx).is_none() {
            return;
        }
        self.request_fetch(ctx, WorkRequest::FetchJellyfinViews);
    }
    /// Round 92: paint the last known library list from disk before the
    /// server answers, so the Libraries tab is populated on its first frame
    /// instead of staying blank until the fetch returns. The pane then
    /// refreshes in the background and rewrites the cache (`JF_VIEWS`).
    /// A cache written for another server/account is ignored; a missing or
    /// unparsable one simply leaves the tab to fill in from the fetch.
    fn load_cached_views(&mut self, ctx: &Ctx) {
        if !self.views.is_empty() {
            return;
        }
        let Some((base, user_id)) = self
            .load_server(ctx)
            .map(|server| (server.base.clone(), server.user_id.clone()))
        else {
            return;
        };
        let Some(cache) = crate::jellyfin::load_views_cache(ctx.config.cache_dir.as_deref())
        else {
            return;
        };
        if cache.views.is_empty() {
            return;
        }
        // Identity check: the account must match. The server address alone
        // is NOT enough to reject the cache — the same server is reachable
        // through more than one address (a LAN IP and a localhost proxy),
        // and the cached list is identical for both; a genuinely different
        // account still invalidates it.
        if cache.user_id != user_id {
            log::debug!(
                cached_user:? = cache.user_id, user:? = user_id; "Ignoring the Jellyfin views cache (different account)"
            );
            return;
        }
        if cache.server != base {
            log::debug!(
                cached_server:? = cache.server, server:? = base; "Showing the Jellyfin views cache from another address of the same account"
            );
        }
        log::debug!(views = cache.views.len(), saved_at = cache.saved_at; "Showing the cached Jellyfin views");
        self.views = cache.views;
        self.rebuild_tree();
        if self.selected.is_none() {
            if !self.tree.is_empty() {
                self.tree_list.select(Some(0));
            }
            self.populate_items();
            self.sync_poster(ctx);
        } else {
            self.populate_items();
        }
    }
    /// Round 92: one library fetch started — show the loading indicator and
    /// keep a slow redraw tick alive so its spinner animates.
    fn begin_fetch(&mut self, ctx: &Ctx) {
        self.fetches_in_flight = self.fetches_in_flight.saturating_add(1);
        self.fetch_activity_at = std::time::Instant::now();
        if self.loading_since.is_none() {
            self.loading_since = Some(std::time::Instant::now());
        }
        self.schedule_loading_tick(ctx);
    }
    /// Round 92: one tracked fetch finished (result or error).
    fn end_fetch(&mut self, ctx: &Ctx) {
        self.fetches_in_flight = self.fetches_in_flight.saturating_sub(1);
        self.fetch_activity_at = std::time::Instant::now();
        if self.fetches_in_flight == 0 {
            self.loading_since = None;
            if let Some(id) = self.loading_tick.take() {
                ctx.scheduler.cancel(id);
            }
        }
    }
    /// Result ids whose fetches the loading indicator tracks (poster,
    /// info-item and playback fetches are not part of the "library is
    /// loading" signal).
    fn is_tracked_fetch(id: &str) -> bool {
        matches!(
            id,
            JF_VIEWS | JF_FOLDER | JF_ARTISTS | JF_ALBUMS | JF_SONGS | JF_SEARCH
        )
    }
    /// Arm (and re-arm) the single redraw tick that animates the indicator.
    fn schedule_loading_tick(&mut self, ctx: &Ctx) {
        let id = self.loading_tick.unwrap_or_else(|| {
            let id = crate::shared::id::new();
            self.loading_tick = Some(id);
            id
        });
        ctx.scheduler.schedule_replace(
            id,
            std::time::Duration::from_millis(LOADING_FRAME_MS),
            move |(tx, _)| {
                tx.send(crate::AppEvent::RequestRender)?;
                Ok(())
            },
        );
    }
    /// Request one library fetch (round 92: every request goes through
    /// here, so the loading indicator always matches what is in flight; a
    /// send that fails clears the slot again immediately).
    fn request_fetch(&mut self, ctx: &Ctx, request: WorkRequest) {
        self.begin_fetch(ctx);
        if ctx.work_sender.send(request).is_err() {
            log::error!("Failed to request jellyfin data");
            self.end_fetch(ctx);
        }
    }
    /// Lazy-load the children of a node kind.
    fn ensure_loaded(&mut self, kind: &JfNodeKind, ctx: &Ctx) {
        if self.load_server(ctx).is_none() {
            return;
        }
        let request = match kind {
            JfNodeKind::View(item) => {
                if item.is_music_view {
                    (!self.artists.contains_key(&item.id)).then(|| {
                        WorkRequest::FetchJellyfinArtists {
                            view_id: item.id.clone(),
                        }
                    })
                } else {
                    (!self.folders.contains_key(&item.id)).then(|| {
                        WorkRequest::FetchJellyfinFolder {
                            parent_id: item.id.clone(),
                        }
                    })
                }
            }
            JfNodeKind::Artist(item) => {
                (!self.albums.contains_key(&item.id)).then(|| {
                    WorkRequest::FetchJellyfinAlbums {
                        artist_id: item.id.clone(),
                    }
                })
            }
            JfNodeKind::Album(item) => {
                (!self.songs.contains_key(&item.id)).then(|| {
                    WorkRequest::FetchJellyfinSongs {
                        album_id: item.id.clone(),
                    }
                })
            }
            JfNodeKind::Folder(item) => {
                (!self.folders.contains_key(&item.id)).then(|| {
                    WorkRequest::FetchJellyfinFolder {
                        parent_id: item.id.clone(),
                    }
                })
            }
        };
        if let Some(request) = request {
            self.request_fetch(ctx, request);
        }
    }
    /// Children of a node from whatever is loaded (None = still loading).
    fn children_of(&self, kind: &JfNodeKind) -> Option<Vec<JfItem>> {
        match kind {
            JfNodeKind::View(item) if item.is_music_view => {
                self.artists.get(&item.id).cloned()
            }
            JfNodeKind::View(item) => self.folders.get(&item.id).cloned(),
            JfNodeKind::Artist(item) => self.albums.get(&item.id).cloned(),
            JfNodeKind::Album(item) => self.songs.get(&item.id).cloned(),
            JfNodeKind::Folder(item) => self.folders.get(&item.id).cloned(),
        }
    }
    /// Rebuild the visible tree from the loaded data + expansion state.
    fn rebuild_tree(&mut self) {
        let prev_key = self.selected.as_ref().map(JfNodeKind::key);
        let mut tree: Vec<JfNode> = Vec::new();
        for view in &self.views {
            let view_kind = JfNodeKind::View(view.clone());
            let expanded = self.expanded.contains(&view_kind.key());
            tree.push(JfNode {
                kind: view_kind.clone(),
                depth: 0,
                expandable: true,
                expanded,
            });
            if !expanded {
                continue;
            }
            if view.is_music_view {
                for artist in self.artists.get(&view.id).cloned().unwrap_or_default() {
                    let artist_kind = JfNodeKind::Artist(artist.clone());
                    let artist_expanded = self.expanded.contains(&artist_kind.key());
                    tree.push(JfNode {
                        kind: artist_kind.clone(),
                        depth: 1,
                        expandable: true,
                        expanded: artist_expanded,
                    });
                    if artist_expanded {
                        for album in self
                            .albums
                            .get(&artist.id)
                            .cloned()
                            .unwrap_or_default()
                        {
                            tree.push(JfNode {
                                kind: JfNodeKind::Album(album),
                                depth: 2,
                                expandable: false,
                                expanded: false,
                            });
                        }
                    }
                }
            } else {
                for folder in self.folders.get(&view.id).cloned().unwrap_or_default() {
                    self.push_folder_rows(&mut tree, folder, 1);
                }
            }
        }
        self.tree = tree;
        if let Some(key) = prev_key {
            if let Some(idx) = self.tree.iter().position(|n| n.kind.key() == key) {
                self.tree_list.select(Some(idx));
                return;
            }
        }
        self.tree_list.select(if self.tree.is_empty() { None } else { Some(0) });
    }
    fn push_folder_rows(&self, tree: &mut Vec<JfNode>, folder: JfItem, depth: u8) {
        if !folder.is_container() {
            return;
        }
        let folder_kind = folder.kind.clone();
        let folder_id = folder.id.clone();
        let kind = JfNodeKind::Folder(folder);
        let loaded = self.folders.get(&folder_id);
        let leaf_container = folder_kind == "Season";
        let has_subdirs = loaded
            .is_some_and(|kids| kids.iter().any(JfItem::is_container));
        let expandable = !leaf_container && (loaded.is_none() || has_subdirs);
        let expanded = self.expanded.contains(&kind.key()) && has_subdirs;
        tree.push(JfNode {
            kind: kind.clone(),
            depth,
            expandable,
            expanded,
        });
        if expanded {
            for child in loaded.cloned().unwrap_or_default() {
                self.push_folder_rows(tree, child, depth + 1);
            }
        }
    }
    /// Populate the right pane for the selected node. Only containers
    /// (expandable) and playable items (audio/video) are shown.
    /// Populate the right pane for the selected node: always its children
    /// (the tree highlight and the right pane share the same selection).
    /// At the root (`selected` = None) the right pane lists every
    /// library/category.
    fn populate_items(&mut self) {
        self.item_list.select(None);
        self.items = self
            .selected
            .as_ref()
            .map(|kind| self.children_of(kind).unwrap_or_default())
            .unwrap_or_else(|| self.views.clone())
            .into_iter()
            .filter(|item| item.is_container() || item.is_playable())
            .collect();
        if !self.items.is_empty() {
            self.item_list.select(Some(0));
            *self.item_list.offset_mut() = 0;
        }
    }
    /// The node that contains `kind` (the node whose children include it),
    /// or None when `kind` sits directly under the root.
    fn parent_of(&self, kind: &JfNodeKind) -> Option<JfNodeKind> {
        match kind {
            JfNodeKind::View(_) => None,
            JfNodeKind::Artist(item) => {
                self.views
                    .iter()
                    .find(|view| {
                        self.artists
                            .get(&view.id)
                            .is_some_and(|a| a.iter().any(|x| x.id == item.id))
                    })
                    .cloned()
                    .map(JfNodeKind::View)
            }
            JfNodeKind::Album(item) => {
                self.views
                    .iter()
                    .find_map(|view| {
                        self.artists
                            .get(&view.id)
                            .into_iter()
                            .flatten()
                            .find_map(|artist| {
                                self.albums
                                    .get(&artist.id)
                                    .is_some_and(|al| al.iter().any(|x| x.id == item.id))
                                    .then(|| JfNodeKind::Artist(artist.clone()))
                            })
                    })
            }
            JfNodeKind::Folder(item) => {
                for (parent_id, kids) in &self.folders {
                    if kids.iter().any(|x| x.id == item.id) {
                        if let Some(view) = self
                            .views
                            .iter()
                            .find(|v| v.id == *parent_id)
                        {
                            return Some(JfNodeKind::View(view.clone()));
                        }
                        for kids in self.folders.values() {
                            if let Some(folder) = kids
                                .iter()
                                .find(|x| x.id == *parent_id)
                            {
                                return Some(JfNodeKind::Folder(folder.clone()));
                            }
                        }
                    }
                }
                None
            }
        }
    }
    /// Highlight `id` in the right pane (the row we came from when backing
    /// out), falling back to the first row.
    fn select_items_item(&mut self, id: &str) {
        let idx = self.items.iter().position(|item| item.id == id).unwrap_or(0);
        if !self.items.is_empty() {
            self.item_list.select(Some(idx));
        }
    }
    /// Keep the tree highlight on the right-pane cursor: the highlighted
    /// item's row in the tree when it has one (libraries, series, seasons,
    /// ...), otherwise the current node's row (episodes have no tree row).
    fn sync_tree_to_items_cursor(&mut self) {
        let target = self
            .selected_item()
            .map(|item| item.id)
            .or_else(|| self.selected.as_ref().map(|k| k.id().to_owned()));
        let Some(target) = target else { return };
        if let Some(idx) = self
            .tree
            .iter()
            .position(|node| node.kind.item().id == target)
        {
            self.tree_list.select(Some(idx));
        } else if let Some(kind) = self.selected.as_ref()
            && let Some(idx) = self
                .tree
                .iter()
                .position(|node| node.kind.key() == kind.key())
        {
            self.tree_list.select(Some(idx));
        }
    }
    fn select_node(&mut self, kind: &JfNodeKind, ctx: &Ctx) -> Result<()> {
        self.selected = Some(kind.clone());
        self.ensure_loaded(kind, ctx);
        self.populate_items();
        if !self.tree.iter().any(|node| node.kind.key() == kind.key()) {
            self.expand_path_to(kind);
            self.rebuild_tree();
        }
        self.sync_tree_to_items_cursor();
        self.sync_poster(ctx);
        ctx.render()?;
        Ok(())
    }
    /// Expand the tree path (the node and its ancestors) so the currently
    /// opened directory is visible in the left pane.
    fn expand_path_to(&mut self, kind: &JfNodeKind) {
        let mut keys = vec![kind.key()];
        match kind {
            JfNodeKind::View(_) => {}
            JfNodeKind::Artist(item) => {
                for view in &self.views {
                    if self
                        .artists
                        .get(&view.id)
                        .is_some_and(|a| a.iter().any(|artist| artist.id == item.id))
                    {
                        keys.push(JfNodeKind::View(view.clone()).key());
                        break;
                    }
                }
            }
            JfNodeKind::Album(item) => {
                'outer: for view in &self.views {
                    for artist in self.artists.get(&view.id).into_iter().flatten() {
                        if self
                            .albums
                            .get(&artist.id)
                            .is_some_and(|albums| {
                                albums.iter().any(|album| album.id == item.id)
                            })
                        {
                            keys.push(JfNodeKind::Artist(artist.clone()).key());
                            keys.push(JfNodeKind::View(view.clone()).key());
                            break 'outer;
                        }
                    }
                }
            }
            JfNodeKind::Folder(item) => {
                let mut current_id = item.id.clone();
                for _ in 0..8 {
                    let mut parent_key = None;
                    for view in &self.views {
                        if self
                            .folders
                            .get(&view.id)
                            .is_some_and(|kids| kids.iter().any(|f| f.id == current_id))
                        {
                            parent_key = Some(JfNodeKind::View(view.clone()).key());
                            break;
                        }
                    }
                    if let Some(parent) = parent_key {
                        keys.push(parent);
                        break;
                    }
                    let mut found = false;
                    for kids in self.folders.values() {
                        for folder in kids {
                            if folder.id == current_id {
                                continue;
                            }
                            if self
                                .folders
                                .get(&folder.id)
                                .is_some_and(|sub| sub.iter().any(|f| f.id == current_id))
                            {
                                keys.push(JfNodeKind::Folder(folder.clone()).key());
                                current_id = folder.id.clone();
                                found = true;
                                break;
                            }
                        }
                        if found {
                            break;
                        }
                    }
                    if !found {
                        break;
                    }
                }
            }
        }
        for key in keys {
            self.expanded.insert(key);
        }
    }
    /// Round 64: whether the selected item gets the small poster shelf.
    /// Every kind sync_poster fetches an image for (playables + the
    /// season/folder containers) is shelf-worthy; the shelf replaces both
    /// the old full-box art (`image_only`) and the 40%-column split.
    fn poster_relevant(&self) -> bool {
        self.selected_item()
            .is_some_and(|i| i.kind_matches_poster())
            || self
                .selected
                .as_ref()
                .is_some_and(|k| k.item().kind_matches_poster())
    }
    /// Fetch the primary image of the item selected in the right pane (or of
    /// the opened node when nothing is selected) and display it in the info
    /// box.
    fn sync_poster(&mut self, ctx: &Ctx) {
        let target = self
            .selected_item()
            .filter(|item| item.is_playable() || item.is_container())
            .or_else(|| self.selected.as_ref().map(|k| k.item().clone()));
        let Some(target) = target else {
            if self.poster.item_id.is_some() {
                self.poster.clear(ctx);
            }
            return;
        };
        if self.poster.item_id.as_deref() != Some(target.id.as_str()) {
            self.poster.clear(ctx);
            self.poster.item_id = Some(target.id.clone());
            self.poster.fallback_id = if target.kind == "Season" {
                target.series_id.clone()
            } else {
                None
            };
            let _ = ctx
                .work_sender
                .send(WorkRequest::FetchJellyfinImage {
                    item_id: target.id.clone(),
                    fallback_item_id: self.poster.fallback_id.clone(),
                })
                .map_err(|err| {
                    log::error!(error:? = err; "Failed to request jellyfin poster")
                });
        }
    }
    fn set_expanded(
        &mut self,
        kind: &JfNodeKind,
        expanded: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        let key = kind.key();
        if expanded {
            self.expanded.insert(key.clone());
            self.ensure_loaded(kind, ctx);
        } else {
            self.expanded.remove(&key);
        }
        self.rebuild_tree();
        if self.selected.as_ref().is_some_and(|k| k.key() == key) {
            self.populate_items();
            self.sync_poster(ctx);
        }
        ctx.render()?;
        Ok(())
    }
    /// The stream URL for an item id (audio vs video endpoint).
    fn stream_url(&self, item: &JfItem) -> Option<String> {
        self.server
            .as_ref()
            .map(|s| {
                if item.is_audio() {
                    s.stream_url(&item.id)
                } else {
                    s.video_stream_url(&item.id)
                }
            })
    }
    /// Play the given stream URL as a temporary (queue-free) entry.
    /// Play a video item, prompting when a video is already playing in mpv:
    /// a different file switches the running session to it, the same file
    /// offers to restart from the beginning. An episode's "Play with MPV"
    /// builds its whole season as the mpv playlist (starting at the clicked
    /// episode) instead of playing a single file.
    fn play_video(
        ctx: &Ctx,
        url: String,
        name: String,
        item_id: String,
        season_id: Option<String>,
    ) -> Result<()> {
        use crate::ui::modals::confirm_modal::{Action, ConfirmModal};
        if ctx.mpv.active {
            if ctx.mpv.item_id.as_deref() == Some(item_id.as_str()) {
                modal!(
                    ctx, ConfirmModal::builder().ctx(ctx)
                    .message(vec![format!("{name} is already playing."),
                    "Restart it from the beginning?".to_owned(),]).action(Action::Single
                    { on_confirm : Box::new(move | ctx | { if let Some(socket) = ctx.mpv
                    .socket.clone() { crate ::core::mpv::mpv_seek(& socket, 0.0); crate
                    ::core::mpv::mpv_unpause(& socket); } Ok(()) }), confirm_label :
                    Some("Restart"), cancel_label : Some("Cancel"), }).size((50, 6))
                    .build()
                );
            } else {
                let season_id = season_id.clone();
                modal!(
                    ctx, ConfirmModal::builder().ctx(ctx)
                    .message(vec![format!("Play {name} instead of the current video?")])
                    .action(Action::Single { on_confirm : Box::new(move | ctx | { let
                    playlist_idx = ctx.mpv.playlist.borrow().iter().position(| e | {
                    crate ::jellyfin::item_id_from_url(& e.url).is_some_and(| id | id ==
                    item_id) }); if let Some(idx) = playlist_idx && let Some(socket) =
                    ctx.mpv.socket.clone() { crate ::core::mpv::mpv_set_playlist_pos(&
                    socket, idx); } else { if let Some(socket) = ctx.mpv.socket.clone() {
                    crate ::core::mpv::mpv_loadfile(& socket, & url); } if let
                    Some(season_id) = season_id { let _ = ctx.work_sender.send(crate
                    ::shared::events::WorkRequest::FetchJellyfinSeason { season_id,
                    episode_id : item_id.clone(), },); } } let _ = ctx.app_event_sender
                    .send(crate ::AppEvent::UiEvent(crate
                    ::ui::UiAppEvent::MpvItemChanged { item_id, title : name, }),);
                    Ok(()) }), confirm_label : Some("Play"), cancel_label :
                    Some("Cancel"), }).size((50, 6)).build()
                );
            }
            return Ok(());
        }
        let start_episode = {
            let season_id = season_id.clone();
            let item_id = item_id.clone();
            let url = url.clone();
            // Round 64: pass the content name to mpv (the window title
            // previously showed the raw stream URL).
            let name = name.clone();
            move |ctx: &Ctx| {
                if let Some(season_id) = season_id.clone() {
                    let _ = ctx
                        .work_sender
                        .send(crate::shared::events::WorkRequest::FetchJellyfinSeason {
                            season_id,
                            episode_id: item_id.clone(),
                        });
                } else {
                    crate::core::mpv::run_mpv_titled(ctx, &url, &name);
                }
            }
        };
        match ctx.config.video.playback {
            crate::config::video::VideoPlaybackMode::Mpv => start_episode(ctx),
            crate::config::video::VideoPlaybackMode::Mpd => jellyfin_play_temp(ctx, url),
            crate::config::video::VideoPlaybackMode::Ask => {
                let menu = MenuModal::new(ctx)
                    .width(60)
                    .title(format!(" {name} "))
                    .list_section(
                        ctx,
                        |section| {
                            let mut section = section;
                            section = section
                                .item(
                                    "Play with MPV (video)",
                                    move |ctx| {
                                        start_episode(ctx);
                                        Ok(())
                                    },
                                );
                            let mpd_url = url.clone();
                            section = section
                                .item(
                                    "Play audio with MPD",
                                    move |ctx| {
                                        jellyfin_play_temp(ctx, mpd_url);
                                        Ok(())
                                    },
                                );
                            section.add_item("Cancel", |_ctx| Ok(()));
                            Some(section)
                        },
                    )
                    .build();
                modal!(ctx, menu);
            }
        }
        Ok(())
    }
    /// Play the highlighted item. Audio plays through MPD (temporary entry,
    /// like radio stations); video launches per the configured playback mode
    /// (mpv / MPD audio / ask).
    /// Open a highlighted container item: expand it in the tree and show
    /// its children in the right pane (used by Enter, `→` and double-click).
    fn open_item(&mut self, item: JfItem, ctx: &Ctx) -> Result<()> {
        let kind = if item.kind == "MusicArtist" {
            JfNodeKind::Artist(item)
        } else if item.kind == "MusicAlbum" {
            JfNodeKind::Album(item)
        } else if item.kind == "CollectionFolder" {
            JfNodeKind::View(item)
        } else {
            JfNodeKind::Folder(item)
        };
        self.set_expanded(&kind, true, ctx)?;
        self.select_node(&kind, ctx)
    }
    fn play_selected(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.selected_item() else { return Ok(()) };
        if !item.is_playable() {
            return Ok(());
        }
        if let Some(url) = self.stream_url(&item) {
            if item.is_audio() {
                self.play_temp_url(
                    ctx,
                    JF_PLAY,
                    PaneType::Jellyfin {
                        tree: TreeBrowserArgs::default(),
                    },
                    url,
                );
                status_info!("Playing {}", item.name);
            } else {
                Self::play_video(
                    ctx,
                    url,
                    item.name.clone(),
                    item.id.clone(),
                    item.season_id.clone(),
                )?;
            }
        }
        Ok(())
    }
    /// Drop the temporary play song once playback has moved on.
    /// Second (dim) line of an item row.
    fn item_subline(item: &JfItem) -> String {
        let mut parts: Vec<String> = Vec::new();
        match item.kind.as_str() {
            "MusicAlbum" => {
                if let Some(artist) = &item.album_artist {
                    parts.push(artist.clone());
                }
                if let Some(year) = item.year {
                    parts.push(year.to_string());
                }
                if let Some(count) = item.child_count {
                    parts.push(format!("{count} tracks"));
                }
            }
            "MusicArtist" => {
                if let Some(count) = item.child_count {
                    parts.push(format!("{count} albums"));
                }
            }
            "Audio" => {
                if let Some(artist) = &item.artist {
                    parts.push(artist.clone());
                }
                if let Some(album) = &item.album {
                    parts.push(album.clone());
                }
                if let Some(secs) = item.runtime_secs {
                    parts.push(format!("{}:{:02}", secs / 60, secs % 60));
                }
            }
            "Movie" | "Episode" | "Video" => {
                if let Some(year) = item.year {
                    parts.push(year.to_string());
                }
                if let Some(secs) = item.runtime_secs {
                    parts.push(format!("{}:{:02}", secs / 60, secs % 60));
                }
                let mut lang_parts: Vec<String> = Vec::new();
                if !item.audio_languages.is_empty() {
                    lang_parts.push(format!("a: {}", item.audio_languages.join(",")));
                }
                if !item.subtitle_languages.is_empty() {
                    lang_parts.push(format!("s: {}", item.subtitle_languages.join(",")));
                }
                if !lang_parts.is_empty() {
                    parts.push(lang_parts.join(" "));
                }
            }
            _ => {
                if let Some(count) = item.child_count {
                    parts.push(format!("{count} items"));
                }
            }
        }
        parts.join(" · ")
    }
}
impl JellyfinPane {
    /// Display the poster queued by the last render. Called by the event
    /// loop after the frame's buffer flush, so the flush cannot overwrite
    /// the overlay's placeholder cells.
    pub(crate) fn flush_pending_poster(&mut self, ctx: &Ctx) {
        self.poster.flush_pending(ctx);
    }

    /// A full terminal clear deleted every kitty overlay (the cava-row
    /// drop repaint). The poster facade only re-draws when its area
    /// changes, so without this the info-box image would stay blank until
    /// the next selection/tab redraw. Force the next render to re-place
    /// it (identical recovery to the tab re-entry path).
    pub(crate) fn redraw_poster_after_clear(&mut self, ctx: &Ctx) {
        self.poster.redraw_next_render(ctx);
    }
    /// Hide the poster overlay (window resizing / transient state).
    pub(crate) fn hide_pending_poster(&mut self, ctx: &Ctx) {
        self.poster.hide(ctx);
    }
}
impl TreeBrowserCore for JellyfinPane {
    type Item = JfItem;
    fn tree_rows(&self) -> Vec<TreeRowView> {
        self.tree
            .iter()
            .map(|node| TreeRowView {
                label: node.kind.label(),
                depth: node.depth,
                expandable: node.expandable,
                expanded: node.expanded,
                root: false,
            })
            .collect()
    }
    fn tree_selected(&self) -> usize {
        self.tree_list.selected().unwrap_or(0)
    }
    fn tree_list(&self) -> &ListState {
        &self.tree_list
    }
    fn tree_list_mut(&mut self) -> &mut ListState {
        &mut self.tree_list
    }
    fn tree_area(&self) -> Rect {
        self.tree_area
    }
    fn set_tree_area(&mut self, area: Rect) {
        self.tree_area = area;
    }
    fn set_expanded_idx(&mut self, idx: usize, expanded: bool, ctx: &Ctx) -> Result<()> {
        let Some(node) = self.tree.get(idx).cloned() else { return Ok(()) };
        self.set_expanded(&node.kind, expanded, ctx)
    }
    fn items_len(&self) -> usize {
        self.items.len()
    }
    fn items_list(&self) -> &ListState {
        &self.item_list
    }
    fn items_list_mut(&mut self) -> &mut ListState {
        &mut self.item_list
    }
    fn items_scrollbar_drag(&mut self) -> &mut crate::shared::mouse_event::ScrollbarDrag {
        &mut self.item_scrollbar_drag
    }
    /// Round 64: carve a poster shelf out of the items list's bottom
    /// (between the list and the Info box) when a poster-relevant item is
    /// selected and the terminal has room. The poster is drawn there as a
    /// compact overlay instead of filling a 40% column of the Info box
    /// (user feedback: info-box layout + poster/preview rework).
    ///
    /// Round 65: the Info box takes the SAME capped height as the other
    /// library pages (MPD/Playlists/Downloads: info_box_height(2/3) with
    /// the configured cap, default 15 rows) instead of a fixed 33% share,
    /// and the poster shelf is twice as tall (18 rows).
    fn layout_vertical(&mut self, right: Rect) -> (Rect, Rect) {
        let info_h = self.tree_args.info_box_height(right.height * 2 / 3);
        let items_h = right.height.saturating_sub(info_h);
        let [items, info] = Layout::vertical([
            Constraint::Length(items_h),
            Constraint::Length(info_h),
        ])
        .areas(right);
        self.poster_shelf = None;
        // Round 64-2 (user): the preview is centered over the pane (was
        // right-aligned). Round 66: the shelf WIDTH follows the artwork —
        // the image always displays at the full inner height and the box
        // widens for wider images, centered, with a 1-char outline ring
        // each side.
        if self.poster_relevant() {
            let shelf_w = self
                .poster
                .natural_width_cells(POSTER_SHELF_H.saturating_sub(2))
                .saturating_add(4);
            // The shelf reserves artwork + 4 columns so the rendered image
            // (centered inside the shelf inset by one cell) keeps its exact
            // pre-round-67 size/position; the framing outline is gone, so
            // this is just empty padding around the borderless image.
            // The shelf is carved from the items area (never the Info box),
            // so only the items side needs room for it — the Info box keeps
            // the same 15-row cap as the other library pages even when the
            // poster is shown.
            let fits_horizontally = right.width >= shelf_w + POSTER_SHELF_RIGHT_MARGIN * 2;
            if fits_horizontally && items.height > POSTER_SHELF_H + 6 {
                self.poster_shelf = Some(Rect {
                    x: right.x + (right.width - shelf_w) / 2,
                    y: items.y + items.height - POSTER_SHELF_H,
                    width: shelf_w,
                    height: POSTER_SHELF_H,
                });
                let items_shrunk = Rect {
                    height: items.height - POSTER_SHELF_H,
                    ..items
                };
                return (items_shrunk, info);
            }
        }
        (items, info)
    }
    fn items_area(&self) -> Rect {
        self.items_area
    }
    fn set_items_area(&mut self, area: Rect) {
        self.items_area = area;
    }
    fn item_at(&self, idx: usize) -> Option<Self::Item> {
        self.items.get(idx).cloned()
    }
    fn item_row_height(&self) -> u16 {
        2
    }
    fn item_row(&self, idx: usize, hovered: bool, ctx: &Ctx) -> ListItem<'static> {
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        let playing_id = if crate::core::mpv::mpv_is_ui_source(ctx) {
            ctx.mpv.item_id.clone()
        } else {
            ctx.find_current_song_in_queue()
                .and_then(|(_, song)| crate::jellyfin::item_id_from_url(&song.file))
        };
        let item = &self.items[idx];
        let is_playing = playing_id.as_deref() == Some(item.id.as_str());
        let prefix = if item.is_playable() {
            if is_playing { "▶ " } else { "  " }
        } else if item.is_container() {
            "▸ "
        } else {
            "  "
        };
        let name_style = if is_playing {
            ctx.config.theme.current_item_style
        } else {
            base
        };
        let sub_style = if is_playing {
            ctx.config.theme.current_item_style
        } else {
            dim
        };
        let mut lines = vec![
            Line::from(Span::styled(format!("{prefix}{}", item.name), name_style)),
            Line::from(Span::styled(format!("  {}", Self::item_subline(item)),
            sub_style)),
        ];
        if hovered {
            for line in lines.iter_mut() {
                *line = line.clone().patch_style(ctx.config.theme.hovered_item_style);
            }
        }
        ListItem::new(lines)
    }
    fn highlight_tree_node(&mut self, idx: usize, ctx: &Ctx) -> Result<()> {
        let Some(node) = self.tree.get(idx).cloned() else { return Ok(()) };
        self.tree_list.select(Some(idx));
        self.selected = Some(node.kind.clone());
        self.ensure_loaded(&node.kind, ctx);
        self.populate_items();
        self.sync_poster(ctx);
        ctx.render()?;
        Ok(())
    }
    fn select_parent(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(current) = self.selected.clone() else { return Ok(()) };
        let prev_id = current.id().to_owned();
        let prev_key = current.key();
        self.expanded.remove(&prev_key);
        match self.parent_of(&current) {
            Some(parent) => {
                self.selected = Some(parent.clone());
                self.ensure_loaded(&parent, ctx);
                self.rebuild_tree();
                self.populate_items();
                self.select_items_item(&prev_id);
                self.scroll_items_selection_into_view(ctx);
                self.sync_tree_to_items_cursor();
                self.scroll_tree_selection_into_view(ctx);
                self.sync_poster(ctx);
                ctx.render()?;
            }
            None => {
                self.selected = None;
                self.rebuild_tree();
                self.populate_items();
                self.select_items_item(&prev_id);
                self.scroll_items_selection_into_view(ctx);
                self.sync_tree_to_items_cursor();
                self.scroll_tree_selection_into_view(ctx);
                self.sync_poster(ctx);
                ctx.render()?;
            }
        }
        Ok(())
    }
    fn activate_selected(&mut self, ctx: &Ctx) -> Result<()> {
        if self.server.is_none() {
            // Round 51: no credentials yet — Enter / d / → / double-click
            // opens Settings on the Jellyfin sign-in section instead of a
            // silent no-op (the pane otherwise only shows the notice row).
            crate::ui::modals::settings::SettingsModal::open_jellyfin(ctx);
            return Ok(());
        }
        let Some(item) = self.selected_item() else { return Ok(()) };
        if item.is_playable() {
            self.play_selected(ctx)
        } else {
            self.open_item(item, ctx)
        }
    }
    /// Round 51: clicking the (empty) items pane while no Jellyfin
    /// credentials exist opens Settings on the Jellyfin sign-in section —
    /// the pane is otherwise a dead click.
    fn handle_items_left_click(
        &mut self,
        row: usize,
        _event: &MouseEvent,
        ctx: &Ctx,
    ) -> Result<()> {
        if self.server.is_none() && row >= self.items_len() {
            crate::ui::modals::settings::SettingsModal::open_jellyfin(ctx);
            return Ok(());
        }
        // Round 57 (P0): a qualified TreeBrowserCore::...(self, ...) call
        // on a concrete receiver dispatches to THIS override again (UFCS),
        // not to the trait default - infinite recursion; at -O1+ LLVM
        // tail-calls it into a self-jmp loop that freezes the TUI at 100%
        // CPU. Inline the default body instead.
        if row < self.items_len() {
            self.items_list_mut().select(Some(row));
            self.on_items_cursor_moved(ctx)?;
        }
        Ok(())
    }
    fn open_context_menu(
        &mut self,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        let Some(item) = self.selected_item() else { return Ok(()) };
        if !item.is_playable() {
            return Ok(());
        }
        let name = item.name.clone();
        let is_audio_item = item.is_audio();
        let play_url = self.stream_url(&item).unwrap_or_default();
        let item_id = item.id.clone();
        let item_name = item.name.clone();
        let add_url = play_url.clone();
        let append_url = play_url.clone();
        let menu = MenuModal::new(ctx)
            .width(60)
            .title(format!(" {name} "))
            .anchor(anchor)
            .list_section(
                ctx,
                |section| {
                    let mut section = section;
                    if is_audio_item {
                        section = section
                            .item(
                                "Play now",
                                move |ctx| {
                                    ctx.query()
                                        .id(JF_PLAY)
                                        .replace_id(JF_PLAY)
                                        .target(PaneType::Jellyfin {
                                            tree: TreeBrowserArgs::default(),
                                        })
                                        .query(move |client| {
                                            let id = client.add_id(&play_url, None)?;
                                            client.play_id(id)?;
                                            Ok(MpdQueryResult::Any(Box::new(id)))
                                        });
                                    Ok(())
                                },
                            );
                    } else {
                        let mpv_url = play_url.clone();
                        let season_id = item.season_id.clone();
                        section = section
                            .item(
                                "Play with MPV (video)",
                                move |ctx| {
                                    JellyfinPane::play_video(
                                        ctx,
                                        mpv_url.clone(),
                                        item_name.clone(),
                                        item_id.clone(),
                                        season_id.clone(),
                                    )
                                },
                            );
                        section = section
                            .item(
                                "Play audio with MPD",
                                move |ctx| {
                                    jellyfin_play_temp(ctx, play_url.clone());
                                    Ok(())
                                },
                            );
                    }
                    section = section
                        .item(
                            "Add to queue",
                            move |ctx| {
                                ctx.command(move |client| {
                                    client
                                        .add(
                                            &add_url,
                                            Some(crate::mpd::QueuePosition::RelativeAdd(0)),
                                        )?;
                                    Ok(())
                                });
                                Ok(())
                            },
                        );
                    section = section
                        .item(
                            "Append to queue",
                            move |ctx| {
                                ctx.command(move |client| {
                                    client.add(&append_url, None)?;
                                    Ok(())
                                });
                                Ok(())
                            },
                        );
                    section.add_item("Cancel", |_ctx| Ok(()));
                    Some(section)
                },
            )
            .build();
        modal!(ctx, menu);
        Ok(())
    }
    fn render_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let key = ctx.config.theme.preview_label_style;
        let group = ctx.config.theme.preview_metadata_group_style;
        let bold = ratatui::style::Style::default().add_modifier(Modifier::BOLD);
        let white = ratatui::style::Style::default().fg(Color::White);
        let base = ctx
            .config
            .theme
            .text_color
            .map_or_else(
                ratatui::style::Style::default,
                |c| { ratatui::style::Style::default().fg(c) },
            );
        let dim = base.add_modifier(Modifier::DIM);
        let mut header_prefix: Option<String> = None;
        let mut header_title: Option<String> = None;
        let mut header_time = String::new();
        let mut header_episode_left: Vec<Span<'static>> = Vec::new();
        let mut header_episode_right: Vec<Span<'static>> = Vec::new();
        let mut header_desc = false;
        let mut rows: Vec<ListItem> = Vec::new();
        let mut credits: Vec<Line<'static>> = Vec::new();
        let selected = self.selected_item();
        let display_item: Option<JfItem> = selected
            .clone()
            .map(|item| {
                self.full_item.clone().filter(|f| f.id == item.id).unwrap_or(item)
            });
        let video_layout = display_item
            .as_ref()
            .is_some_and(|item| matches!(item.kind.as_str(), "Movie" | "Episode"));
        match display_item {
            Some(item) if video_layout => {
                let name = if item.kind == "Episode" {
                    item.series_name.clone().unwrap_or_else(|| item.name.clone())
                } else {
                    item.name.clone()
                };
                match item.year {
                    Some(year) => {
                        header_prefix = Some(format!("{year} -- "));
                        header_title = Some(name);
                    }
                    None => header_title = Some(name),
                }
                if let Some(secs) = item.runtime_secs {
                    header_time = format!(
                        "Time: {}", crate ::ui::panes::lyrics::format_clock(secs)
                    );
                }
                if item.kind == "Episode" {
                    header_episode_left.push(Span::styled("Episode: ", base));
                    header_episode_left.push(Span::styled(item.name.clone(), base));
                    if let (Some(season), Some(episode)) = (
                        item.season_number,
                        item.index_number,
                    ) {
                        header_episode_right
                            .push(
                                Span::styled(format!("S{season:02}E{episode:02}"), base),
                            );
                    }
                }
                header_desc = item
                    .overview
                    .as_deref()
                    .is_some_and(|d| !d.trim().is_empty()) || item.director.is_some()
                    || item.writer.is_some() || !item.starring.is_empty();
                if let Some(overview) = item
                    .overview
                    .as_deref()
                    .filter(|d| !d.trim().is_empty())
                {
                    // Round 64: full-width text (the poster column is gone),
                    // wrapped to fit the one-cell side margins.
                    let text_width = (area.width.saturating_sub(6)).max(10) as usize;
                    for line in crate::ui::widgets::wrap::wrap_to_width(
                        &crate::ui::panes::lyrics::scrub_emoji(overview),
                        text_width,
                    ) {
                        rows.push(
                            ListItem::new(
                                Line::from(
                                    Span::styled(line, ctx.config.as_list_text_style()),
                                ),
                            ),
                        );
                    }
                }
                if let Some(director) = &item.director {
                    credits
                        .push(
                            Line::from(
                                vec![
                                    Span::styled("Director: ", key), Span::styled(director
                                    .clone(), white),
                                ],
                            ),
                        );
                }
                if let Some(writer) = &item.writer {
                    credits
                        .push(
                            Line::from(
                                vec![
                                    Span::styled("Writer: ", key), Span::styled(writer.clone(),
                                    white),
                                ],
                            ),
                        );
                }
                if !item.starring.is_empty() {
                    credits
                        .push(
                            Line::from(
                                vec![
                                    Span::styled("Starring: ", key), Span::styled(item.starring
                                    .join(", "), white),
                                ],
                            ),
                        );
                }
                // Round 64: a `----` separator between the description body
                // and the credits block (user's Info-box mockup).
                if !credits.is_empty() {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                Span::styled(
                                    " ----",
                                    dim,
                                ),
                            ),
                        ),
                    );
                }
            }
            Some(item) => {
                rows.push(ListItem::new(Line::styled(" --- [Item]", group)));
                rows.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Name", key), Span::raw(": "), Span::raw(item
                                .name.clone()),
                            ],
                        ),
                    ),
                );
                if let Some(artist) = &item.artist {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Artist", key), Span::raw(": "),
                                    Span::raw(artist.clone()),
                                ],
                            ),
                        ),
                    );
                }
                if let Some(artist) = &item.album_artist {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Album artist", key), Span::raw(": "),
                                    Span::raw(artist.clone()),
                                ],
                            ),
                        ),
                    );
                }
                if let Some(album) = &item.album {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Album", key), Span::raw(": "), Span::raw(album
                                    .clone()),
                                ],
                            ),
                        ),
                    );
                }
                if let Some(year) = item.year {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Year", key), Span::raw(": "), Span::raw(year
                                    .to_string()),
                                ],
                            ),
                        ),
                    );
                }
                if let Some(secs) = item.runtime_secs {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Duration", key),
                                    Span::raw(format!(": {}:{:02}", secs / 60, secs % 60)),
                                ],
                            ),
                        ),
                    );
                }
                rows.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Type", key), Span::raw(": "), Span::raw(item
                                .kind.clone()),
                            ],
                        ),
                    ),
                );
            }
            None => {
                rows.push(ListItem::new(Line::styled(" --- [Server]", group)));
                if let Some(err) = &self.error {
                    rows.push(ListItem::new(Line::styled(err.clone(), dim)));
                } else {
                    let label = self
                        .selected
                        .as_ref()
                        .map(|k| k.label())
                        .unwrap_or_else(|| "No library selected".to_owned());
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Library", key), Span::raw(": "),
                                    Span::raw(label),
                                ],
                            ),
                        ),
                    );
                    if self.items.is_empty() {
                        rows.push(
                            ListItem::new(
                                Line::from(
                                    vec![
                                        Span::styled("Hint", key), Span::raw(": "),
                                        Span::styled("Pick a library on the left.", dim),
                                    ],
                                ),
                            ),
                        );
                    }
                }
            }
        }
        if let Some((_, song)) = ctx.find_current_song_in_queue()
            && let Some(server) = &self.server && song.file.starts_with(&server.base)
            && ctx.status.state == State::Play
        {
            rows.push(ListItem::new(""));
            rows.push(ListItem::new(Line::styled(" --- [Now playing]", group)));
            if let Some(title) = song.metadata.get("title") {
                rows.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Title", key), Span::raw(": "), Span::raw(title
                                .join(", ").into_owned()),
                            ],
                        ),
                    ),
                );
            }
            if let Some(artist) = song.metadata.get("artist") {
                rows.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Artist", key), Span::raw(": "),
                                Span::raw(artist.join(", ").into_owned()),
                            ],
                        ),
                    ),
                );
            }
        }
        let block = ratatui::widgets::Block::default()
            .borders(Borders::ALL)
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        let inner = block.inner(area);
        // Round 64: the poster/preview moved out of the Info box into the
        // small shelf beside the list (see `layout_vertical`); the Info
        // text reclaims the whole inner width. When the terminal is too
        // short for the shelf the poster is hidden instead of re-tiling
        // the Info box.
        if !self.is_modal_open {
            match self.poster_shelf {
                // Round 67 (user): no framing border around the preview —
                // the adaptive border resized while the artwork loaded and
                // was distracting. The image keeps its exact size/position
                // (drawn in the same area as before, just without the box).
                Some(shelf) => {
                    let inner = Rect {
                        x: shelf.x.saturating_add(1),
                        y: shelf.y.saturating_add(1),
                        width: shelf.width.saturating_sub(2),
                        height: shelf.height.saturating_sub(2),
                    };
                    if inner.width >= 3 && inner.height >= 3 {
                        self.poster.draw(inner, ctx);
                    } else {
                        self.poster.hide(ctx);
                    }
                }
                None => self.poster.hide(ctx),
            }
        }
        // Round 64-2 (user): the Info text keeps a one-cell margin on both
        // sides (the mockup's `│ Description ↴` / indented description).
        let text_area = Rect {
            x: inner.x.saturating_add(1),
            y: inner.y,
            width: inner.width.saturating_sub(2),
            height: inner.height,
        };
        let header_h = usize::from(header_title.is_some())
            + usize::from(
                !header_episode_left.is_empty() || !header_episode_right.is_empty(),
            ) + usize::from(header_desc);
        let credits_h = credits.len();
        let (header_area, body_area, credits_area) = if header_h > 0
            && header_h + credits_h < text_area.height as usize
        {
            let [a, b, c] = Layout::vertical([
                    Constraint::Length(header_h as u16),
                    Constraint::Min(0),
                    Constraint::Length(credits_h as u16),
                ])
                .areas(text_area);
            (a, b, c)
        } else {
            (Rect::default(), text_area, Rect::default())
        };
        if header_h > 0 && header_area.height > 0 {
            let time_w = (header_time.chars().count() + 4) as u16;
            let (title_area, time_area) = {
                let [a, b] = Layout::horizontal([
                        Constraint::Min(0),
                        Constraint::Length(time_w),
                    ])
                    .areas(header_area);
                (a, b)
            };
            let prefix = header_prefix.unwrap_or_default();
            let prefix_w = prefix.chars().count() as u16;
            let (prefix_area, marquee_area) = if prefix_w > 0
                && prefix_w < title_area.width
            {
                let [a, b] = Layout::horizontal([
                        Constraint::Length(prefix_w),
                        Constraint::Min(0),
                    ])
                    .areas(title_area);
                (a, b)
            } else {
                (Rect::default(), title_area)
            };
            if prefix_w > 0 {
                frame
                    .render_widget(
                        Paragraph::new(Span::styled(prefix, base)),
                        prefix_area,
                    );
            }
            let title = header_title.unwrap_or_default();
            let title_len = title.chars().count() as u16;
            let offset = if title_len > marquee_area.width {
                let elapsed_ms = self
                    .info_song_shown_at
                    .map(|t| t.elapsed().as_millis())
                    .unwrap_or(0) as u64;
                crate::ui::widgets::marquee::marquee_offset(
                    elapsed_ms,
                    title_len,
                    marquee_area.width,
                )
            } else {
                0
            };
            crate::ui::widgets::marquee::draw_panel_at(
                frame.buffer_mut(),
                marquee_area.x,
                marquee_area.y,
                marquee_area.width,
                &Line::from(Span::styled(title, base)),
                offset,
                base,
            );
            if !header_time.is_empty() {
                frame
                    .render_widget(
                        Paragraph::new(Span::styled(header_time, bold))
                            .alignment(Alignment::Right),
                        time_area,
                    );
            }
            let has_episode = !header_episode_left.is_empty()
                || !header_episode_right.is_empty();
            if has_episode {
                let row = Rect {
                    x: header_area.x,
                    y: header_area.y + 1,
                    width: header_area.width,
                    height: 1,
                };
                let [left_area, right_area] = Layout::horizontal([
                        Constraint::Min(0),
                        Constraint::Length(time_w),
                    ])
                    .areas(row);
                if !header_episode_left.is_empty() {
                    frame
                        .render_widget(
                            Paragraph::new(Line::from(header_episode_left)),
                            left_area,
                        );
                }
                if !header_episode_right.is_empty() {
                    frame
                        .render_widget(
                            Paragraph::new(Line::from(header_episode_right))
                                .alignment(Alignment::Right),
                            right_area,
                        );
                }
            }
            if header_desc {
                let y = header_area.y + 1 + u16::from(has_episode);
                let row = Rect {
                    x: header_area.x,
                    y,
                    width: header_area.width,
                    height: 1,
                };
                frame
                    .render_widget(
                        Paragraph::new(
                            Line::from(
                                vec![
                                    Span::styled("Description", key), Span::styled(" ↴",
                                    white),
                                ],
                            ),
                        ),
                        row,
                    );
            }
        }
        for (i, line) in credits.iter().enumerate() {
            let row = Rect {
                x: credits_area.x,
                y: credits_area.y + i as u16,
                width: credits_area.width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(line.clone()), row);
        }
        self.info_items_len = rows.len();
        let selection_key = self
            .selected_item()
            .map(|i| i.id)
            .or_else(|| self.selected.as_ref().map(|k| k.id().to_owned()));
        if self.info_song_id != selection_key {
            self.info_song_id = selection_key.clone();
            self.info_state = ListState::default();
            self.info_song_shown_at = Some(std::time::Instant::now());
            self.full_item = None;
            if let Some(id) = selection_key
                && self
                    .selected_item()
                    .is_some_and(|i| matches!(i.kind.as_str(), "Movie" | "Episode"))
            {
                let _ = ctx
                    .work_sender
                    .send(crate::shared::events::WorkRequest::FetchJellyfinItem {
                        item_id: id,
                    });
            }
        }
        if rows.is_empty() || body_area.height == 0 {
            frame.render_widget(block, area);
            self.info_area = body_area;
            self.info_scrollbar_area = Rect::default();
            return;
        }
        let overflow = rows.len() > body_area.height as usize;
        let (list_area, scrollbar_area) = if overflow
            && ctx.config.as_styled_scrollbar().is_some()
        {
            crate::ui::scrollbar_strip(body_area)
        } else {
            (body_area, Rect::default())
        };
        ratatui::widgets::StatefulWidget::render(
            List::new(rows).style(base),
            list_area,
            frame.buffer_mut(),
            &mut self.info_state,
        );
        if scrollbar_area.width > 0
            && let Some(scrollbar) = ctx.config.as_styled_scrollbar()
        {
            let max_offset = self
                .info_items_len
                .saturating_sub(list_area.height as usize);
            let position = self.info_state.offset().min(max_offset);
            crate::ui::render_scrollbar_strip(
                frame,
                scrollbar,
                scrollbar_area,
                &mut ratatui::widgets::ScrollbarState::new(max_offset + 1)
                    .position(position)
                    .viewport_content_length(list_area.height as usize),
            );
        }
        frame.render_widget(block, area);
        self.info_area = list_area;
        self.info_scrollbar_area = scrollbar_area;
    }
    fn temp_play_id(&self) -> Option<u32> {
        self.temp_play_id
    }
    fn set_temp_play_id(&mut self, id: Option<u32>) {
        self.temp_play_id = id;
    }
    fn tree_title(&self) -> &'static str {
        " Libraries "
    }
    fn items_title(&self) -> String {
        self.selected
            .as_ref()
            .map(|kind| format!(" {} ", kind.label()))
            .unwrap_or_else(|| " Items ".to_owned())
    }
    /// The configured tree-browser args drive the shared `split_tree`
    /// (tree min width / hide threshold).
    fn tree_args(&self) -> TreeBrowserArgs {
        self.tree_args.clone()
    }
    fn sync_tree_to_items_cursor(&mut self) {
        let target = self
            .selected_item()
            .map(|item| item.id)
            .or_else(|| self.selected.as_ref().map(|k| k.id().to_owned()));
        let Some(target) = target else { return };
        if let Some(idx) = self
            .tree
            .iter()
            .position(|node| node.kind.item().id == target)
        {
            self.tree_list.select(Some(idx));
        } else if let Some(kind) = self.selected.as_ref()
            && let Some(idx) = self
                .tree
                .iter()
                .position(|node| node.kind.key() == kind.key())
        {
            self.tree_list.select(Some(idx));
        }
    }
    fn on_items_cursor_moved(&mut self, ctx: &Ctx) -> Result<()> {
        let before = self.tree_selected();
        self.sync_tree_to_items_cursor();
        if self.tree_selected() != before {
            self.scroll_tree_selection_into_view(ctx);
        }
        self.sync_poster(ctx);
        ctx.render()?;
        Ok(())
    }
    /// Round 32: the wheel scrolls the viewport only in Queue, Playlists,
    /// MPD, Help and Radio — Jellyfin is NOT in the round-32 pane list, so
    /// it keeps the wheel-moves-selection behavior.
    fn wheel_scrolls_viewport(&self) -> bool {
        false
    }
    fn on_tree_focus(&mut self) {
        self.focus = PaneFocus::Tree;
    }
    fn on_items_focus(&mut self) {
        self.focus = PaneFocus::Items;
    }
    /// The tree pane is hidden on TUIs ≤ 120 columns wide: reset the tree
    /// rect so mouse events (scroll included) can never hit the collapsed
    /// pane.
    fn on_tree_hidden(&mut self) {
        self.tree_area = Rect::default();
    }
    /// Double-click on a tree row: expand/collapse expandable nodes, open
    /// leaf containers (e.g. a season).
    fn on_tree_double_click(&mut self, idx: usize, ctx: &Ctx) -> Result<()> {
        self.highlight_tree_node(idx, ctx)?;
        let Some(node) = self.tree.get(idx).cloned() else { return Ok(()) };
        if node.expandable {
            self.set_expanded(&node.kind, !node.expanded, ctx)?;
        } else {
            self.set_expanded(&node.kind, true, ctx)?;
        }
        Ok(())
    }
    fn on_reconnected(&mut self, ctx: &Ctx) -> Result<()> {
        self.initialized = false;
        self.temp_play_id = None;
        self.before_show(ctx)?;
        Ok(())
    }
}


impl JellyfinPane {
    // ── search mode (round 60 B2) ────────────────────────────────────

    /// The selected search result, if any.
    fn search_selected(&self) -> Option<&JfItem> {
        self.search_results_state
            .selected()
            .and_then(|idx| self.search_results.get(idx))
    }

    /// Flip Libraries <-> Search (Shift+Tab / toggle click). The search
    /// state stays for the session.
    ///
    /// Round 73.2: the flip no longer focuses the query input (the `S` key
    /// does that) — it always RELEASES it instead. Releasing is required,
    /// not just optional: `search_input_focused` drives this pane's
    /// keyboard phase (Up/Down move the results only while the input is
    /// NOT focused), so a flip that left the flag true without insert mode
    /// would freeze the results list.
    fn toggle_mode(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.mode = match self.mode {
            JellyfinTabMode::Libraries => JellyfinTabMode::Search,
            JellyfinTabMode::Search => JellyfinTabMode::Libraries,
        };
        self.release_search_input(ctx);
        ctx.render()?;
        Ok(())
    }
    /// Round 73.2 (revision B: `S`): the target of the search key — show the
    /// search page with the query input focused (the caret in the field).
    /// The mode is SET, not toggled, so the key always lands on Search;
    /// nothing is cleared, so the session query and its results are still
    /// there when the page returns.
    fn jump_to_search(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.mode = JellyfinTabMode::Search;
        self.focus_search_input(ctx);
        ctx.render()?;
        Ok(())
    }
    /// Round 62.1 (S1/S2 parity): leave the search page back to the
    /// Jellyfin browser — clears the query + results and returns to
    /// keyboard navigation.
    fn exit_search(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.mode = JellyfinTabMode::Libraries;
        self.release_search_input(ctx);
        self.search_results_state.select(None);
        self.search_results.clear();
        ctx.input.clear_buffer(self.search_buffer);
        ctx.render()?;
        Ok(())
    }
    /// Focus the search input row: the buffer becomes the active
    /// insert-mode buffer so printable keys type into the query (round
    /// 60b D4 — the buffer was never activated before, so typing reached
    /// no handler and `SearchHints` never fired).
    fn focus_search_input(&mut self, ctx: &Ctx) {
        self.search_input_focused = true;
        self.search_left_presses = 0;
        ctx.input.insert_mode(self.search_buffer);
    }
    /// Leave the input (results / mode toggle / tab switch): drop insert
    /// mode when this pane's buffer is the active one — the input manager
    /// is global, so an unattended insert buffer would swallow keys meant
    /// for other panes, tabs and modals.
    fn release_search_input(&mut self, ctx: &Ctx) {
        self.search_input_focused = false;
        if ctx.input.is_active(self.search_buffer) {
            ctx.input.normal_mode();
        }
    }

    /// Fire a server-side search for the current query (deduped; an empty
    /// query clears the results). Results arrive as `JF_SEARCH`.
    fn run_search(&mut self, ctx: &Ctx) {
        let query = ctx.input.value(self.search_buffer).trim().to_owned();
        if query.is_empty() {
            self.search_results.clear();
            self.search_results_state = ListState::default();
            let _ = ctx.render();
            return;
        }
        if self.search_pending {
            return;
        }
        if self.load_server(ctx).is_none() {
            return;
        }
        self.search_pending = true;
        self.last_sent_query = query.clone();
        self.request_fetch(ctx, WorkRequest::FetchJellyfinSearch { query });
    }

    /// `d`/`→`/double-click on a result: play it with the existing
    /// Jellyfin behavior (audio via MPD temp stream, video via mpv).
    fn search_activate(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.search_selected().cloned() else { return Ok(()) };
        if !item.is_playable() {
            // Containers just select (their subtree is browsable in
            // Libraries mode); nothing to play.
            return Ok(());
        }
        let Some(server) = self.load_server(ctx) else {
            return Ok(());
        };
        if item.is_audio() {
            let url = server.stream_url(&item.id);
            self.play_temp_url(
                ctx,
                JF_PLAY,
                PaneType::Jellyfin {
                    tree: TreeBrowserArgs::default(),
                },
                url,
            );
            status_info!("Playing {}", item.name);
        } else {
            let url = server.video_stream_url(&item.id);
            Self::play_video(
                ctx,
                url,
                item.name.clone(),
                item.id.clone(),
                item.season_id.clone(),
            )?;
        }
        Ok(())
    }

    /// The search results' options menu (Enter / right-click): the same
    /// play / add-to-queue / replace-queue actions as the item menu.
    fn search_context_menu(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.search_selected().cloned() else { return Ok(()) };
        if !item.is_playable() {
            return Ok(());
        }
        let name = item.name.clone();
        let Some(server) = self.load_server(ctx) else {
            return Ok(());
        };
        let play_url = if item.is_audio() {
            server.stream_url(&item.id)
        } else {
            server.video_stream_url(&item.id)
        };
        let item_id = item.id.clone();
        let item_name = item.name.clone();
        let is_audio_item = item.is_audio();
        let add_url = play_url.clone();
        let append_url = play_url.clone();
        let menu = MenuModal::new(ctx)
            .width(60)
            .title(format!(" {name} "))
            .list_section(
                ctx,
                |section| {
                    let mut section = section;
                    if is_audio_item {
                        section = section
                            .item(
                                "Play now",
                                move |ctx| {
                                    ctx.query()
                                        .id(JF_PLAY)
                                        .replace_id(JF_PLAY)
                                        .target(PaneType::Jellyfin {
                                            tree: TreeBrowserArgs::default(),
                                        })
                                        .query(move |client| {
                                            let id = client.add_id(&play_url, None)?;
                                            client.play_id(id)?;
                                            Ok(MpdQueryResult::Any(Box::new(id)))
                                        });
                                    Ok(())
                                },
                            );
                    } else {
                        let mpv_url = play_url.clone();
                        let season_id = item.season_id.clone();
                        section = section
                            .item(
                                "Play with MPV (video)",
                                move |ctx| {
                                    JellyfinPane::play_video(
                                        ctx,
                                        mpv_url.clone(),
                                        item_name.clone(),
                                        item_id.clone(),
                                        season_id.clone(),
                                    )
                                },
                            );
                    }
                    section = section
                        .item("Add to queue", move |ctx| {
                            let add_url = add_url.clone();
                            ctx.command(move |client| {
                                client.enqueue_multiple(
                                    vec![Enqueue::File { path: add_url }],
                                    None,
                                    None,
                                    false,
                                )?;
                                Ok(())
                            });
                            Ok(())
                        })
                        .item("Replace queue", move |ctx| {
                            let append_url = append_url.clone();
                            ctx.command(move |client| {
                                client.enqueue_multiple(
                                    vec![Enqueue::File { path: append_url }],
                                    None,
                                    None,
                                    true,
                                )?;
                                Ok(())
                            });
                            Ok(())
                        });
                    Some(section)
                },
            )
            .list_section(
                ctx,
                |section| {
                    let section = section.item("Cancel", |_ctx| Ok(()));
                    Some(section)
                },
            )
            .build();
        crate::shared::macros::modal!(ctx, menu);
        Ok(())
    }

    /// Search-mode keys (parity with the MPD/playlists search): `d`/`→`
    /// move from the input into the results, `a`/`←` return, Enter opens
    /// the options menu, Esc clears nothing (results are not markable
    /// here — the list keeps single selection).
    fn handle_search_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_directories() {
            match action {
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    if self.search_input_focused && !self.search_results.is_empty() {
                        self.release_search_input(ctx);
                        ctx.render()?;
                    } else if !self.search_input_focused {
                        self.search_activate(ctx)?;
                    }
                    return Ok(());
                }
                DirectoriesActions::FolderCollapse => {
                    if self.search_input_focused {
                        // Round 62.1 (S2): Left #2 from the search bar
                        // behaves exactly like Esc #2 — leave the search
                        // page back to the Jellyfin browser.
                        self.exit_search(ctx)?;
                    } else {
                        self.focus_search_input(ctx);
                        ctx.render()?;
                    }
                    return Ok(());
                }
                _ => event.abandon(),
            }
        }
        if let Some(action) = event.claim_common() {
            match action {
                CommonAction::Up | CommonAction::Down if !self.search_input_focused => {
                    if self.search_results.is_empty() {
                        return Ok(());
                    }
                    let len = self.search_results.len();
                    let sel = self.search_results_state.selected().unwrap_or(0);
                    let next = if matches!(action, CommonAction::Up) {
                        sel.saturating_sub(1)
                    } else {
                        (sel + 1).min(len - 1)
                    };
                    self.search_results_state.select(Some(next));
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::Right if !self.search_results.is_empty() => {
                    self.release_search_input(ctx);
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::Left if !self.search_input_focused => {
                    self.focus_search_input(ctx);
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::Confirm => {
                    return self.search_context_menu(ctx);
                }
                CommonAction::ContextMenu => {
                    return self.search_context_menu(ctx);
                }
                CommonAction::Close => {
                    // Round 62.1 (S1/S2): staged search exit — Esc #1 from
                    // results goes back to the search bar, Esc #2 from the
                    // bar leaves the search page; both are consumed so the
                    // app-level ShowSettings half never fires.
                    if self.search_input_focused {
                        self.exit_search(ctx)?;
                    } else {
                        self.focus_search_input(ctx);
                        ctx.render()?;
                    }
                    event.consume();
                    return Ok(());
                }
                _ => event.abandon(),
            }
        }
        Ok(())
    }

    /// Search-mode mouse: clicks on the input row focus it; clicks on the
    /// results select; double-click plays; right-click opens the menu;
    /// the wheel moves the selection.
    fn handle_search_mouse(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let position: ratatui::layout::Position = event.into();
        if matches!(event.kind, MouseEventKind::LeftClick)
            && self.search_results_area.y > 0
            && self.toggle_areas[0].y > 0
            && position.y > self.toggle_areas[0].y
            && position.y < self.search_results_area.y
        {
            self.focus_search_input(ctx);
            ctx.render()?;
            return Ok(());
        }
        let area = self.search_results_area;
        if !area.contains(position) {
            return Ok(());
        }
        let row = usize::from(event.y.saturating_sub(area.y));
        match event.kind {
            MouseEventKind::LeftClick => {
                if row < self.search_results.len() {
                    self.search_results_state.select(Some(row));
                    self.release_search_input(ctx);
                    ctx.render()?;
                }
            }
            MouseEventKind::DoubleClick => {
                if row < self.search_results.len() {
                    self.search_results_state.select(Some(row));
                    self.search_activate(ctx)?;
                }
            }
            MouseEventKind::RightClick => {
                if row < self.search_results.len() {
                    self.search_results_state.select(Some(row));
                    self.release_search_input(ctx);
                    return self.search_context_menu(ctx);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if self.search_results.is_empty() {
                    return Ok(());
                }
                let len = self.search_results.len();
                let sel = self.search_results_state.selected().unwrap_or(0);
                let next = if matches!(event.kind, MouseEventKind::ScrollUp) {
                    sel.saturating_sub(1)
                } else {
                    (sel + 1).min(len - 1)
                };
                self.search_results_state.select(Some(next));
                ctx.render()?;
            }
            _ => {}
        }
        Ok(())
    }

fn render_toggle(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        self.toggle_areas = [Rect::default(); 2];
        self.back_area = Rect::default();
        if area.height == 0 {
            return;
        }
        let segments = [
            crate::ui::widgets::sub_tab_bar::Segment {
                label: "Libraries",
                active: self.mode == JellyfinTabMode::Libraries,
            },
            crate::ui::widgets::sub_tab_bar::Segment {
                label: "Search",
                active: self.mode == JellyfinTabMode::Search,
            },
        ];
        let x = area.x.saturating_add(1);
        let bar = crate::ui::widgets::sub_tab_bar::SubTabBar::new(
            &segments,
            x,
            area.y,
            area.right().saturating_sub(1),
        );
        let areas = bar.render(frame, ctx);
        for (idx, seg_area) in areas.into_iter().take(2).enumerate() {
            self.toggle_areas[idx] = seg_area;
        }
        // Round 60 (B3): `↰ Back` at the right end of the row, visible
        // only inside a folder/collection (hidden at the libraries root
        // and in search mode).
        let visible = self.mode == JellyfinTabMode::Libraries && self.selected.is_some();
        self.back_area = crate::ui::draw_back_button(frame, area, visible, ctx);
        self.draw_loading(frame, area, ctx);
    }
    /// Round 92: the animated loading indicator, right-aligned on the
    /// mode-toggle row (`⭘ Libraries ● Search`). It ends one blank column
    /// left of `↰ Back` when that button is visible, degrades to the spinner
    /// glyph alone when the label does not fit, and draws nothing at all
    /// rather than overwriting a mode label. The redraw tick that keeps the
    /// spinner moving is re-armed here, so the animation costs nothing while
    /// no fetch is in flight.
    fn draw_loading(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let Some(since) = self.loading_since else { return };
        if self.fetches_in_flight == 0 || area.height == 0 {
            return;
        }
        // A tracked fetch that never reports back (dropped result, hung work
        // thread) must not leave the indicator spinning forever.
        if self.fetch_activity_at.elapsed() > LOADING_WATCHDOG {
            log::warn!(
                in_flight = self.fetches_in_flight; "Jellyfin fetch never reported back; clearing the loading indicator"
            );
            self.fetches_in_flight = 0;
            self.loading_since = None;
            if let Some(id) = self.loading_tick.take() {
                ctx.scheduler.cancel(id);
            }
            return;
        }
        self.schedule_loading_tick(ctx);
        let frame_idx = (since.elapsed().as_millis() as u64 / LOADING_FRAME_MS) as usize
            % LOADING_FRAMES.len();
        let spinner = LOADING_FRAMES[frame_idx];
        // The mode labels own the left end of the row; the indicator never
        // reaches into them.
        let labels_end = self.toggle_areas[0]
            .right()
            .max(self.toggle_areas[1].right())
            .max(area.x);
        let right = if self.back_area.width > 0 {
            self.back_area.x.saturating_sub(1)
        } else {
            area.right().saturating_sub(1)
        };
        let style = ctx.config.as_list_text_style();
        let full = format!("{spinner} {LOADING_LABEL}");
        let full_w =
            unicode_width::UnicodeWidthStr::width(full.as_str()).min(u16::MAX as usize) as u16;
        // One blank column of slack on either side of the indicator.
        let (text, width) = if right >= labels_end + full_w + 2 {
            (full, full_w)
        } else if right >= labels_end + 2 {
            (spinner.to_owned(), 1)
        } else {
            return;
        };
        let x = right.saturating_sub(width);
        if x <= labels_end {
            return;
        }
        frame.render_widget(
            ratatui::widgets::Paragraph::new(text).style(style),
            Rect { x, y: area.y, width, height: 1 },
        );
    }
    /// Search mode (round 60 B2): `Search:` input row + separator, the
    /// scrollable results list and the info box.
    fn render_search(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        // Round 60c (S1): ONE combined frame (same shared layout as the
        // Playlists search) — `Search:` input row on top, connected
        // `├───…───┤` divider, results list in the same box, `Results`
        // on the bottom edge (`╰─Results───…──╯`). The Info Box stays
        // below.
        let [frame_area, info_area] = Layout::vertical([
                Constraint::Min(4),
                Constraint::Percentage(33),
            ])
            .areas(area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style())
            .title_bottom(ratatui::text::Line::from("─Results"));
        let inner = block.inner(frame_area);
        // Render the box first so the connected divider's `├`/`┤`
        // junctions can overwrite the box's border cells at that row.
        frame.render_widget(block, frame_area);
        let query = ctx.input.value(self.search_buffer);
        let content = crate::ui::render_search_frame_top(
            frame,
            inner,
            &query,
            self.search_input_focused,
            ctx,
        );
        let (list_area, scrollbar_area) = if ctx.config.theme.scrollbar.is_some() {
            crate::ui::scrollbar_strip(content)
        } else {
            (content, Rect::default())
        };
        self.search_results_area = list_area;
        let list = List::new(search_result_rows(&self.search_results))
            .style(ctx.config.as_list_name_style())
            .highlight_style(
                if !self.search_input_focused {
                    ctx.config.theme.hovered_item_style
                } else {
                    ctx.config.theme.current_item_style
                },
            );
        ratatui::widgets::StatefulWidget::render(
            list,
            list_area,
            frame.buffer_mut(),
            &mut self.search_results_state,
        );
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
            && scrollbar_area.width > 0
        {
            let max_offset =
                self.search_results.len().saturating_sub(list_area.height as usize);
            let position = self.search_results_state.offset().min(max_offset);
            crate::ui::render_scrollbar_strip(
                frame,
                scrollbar,
                scrollbar_area,
                &mut ratatui::widgets::ScrollbarState::new(max_offset + 1)
                    .position(position),
            );
        }
        self.render_search_info(frame, info_area, ctx);
        Ok(())
    }
    /// The search info box: the selected result's details (the standard
    /// key-value layout, no poster).
    fn render_search_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let mut items: Vec<ListItem> = Vec::new();
        if let Some(item) = self.search_selected() {
            let key_style = ctx.config.theme.preview_label_style;
            let group = ctx.config.theme.preview_metadata_group_style;
            items.push(ListItem::new(Line::styled(" --- [Item]", group)));
            items.push(
                ListItem::new(
                    Line::from(
                        vec![
                            Span::styled("Title", key_style), Span::raw(": "),
                            Span::raw(item.name.clone()),
                        ],
                    ),
                ),
            );
            if let Some(artist) = &item.artist {
                items.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Artist", key_style), Span::raw(": "),
                                Span::raw(artist.clone()),
                            ],
                        ),
                    ),
                );
            }
            if let Some(album) = &item.album {
                items.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Album", key_style), Span::raw(": "),
                                Span::raw(album.clone()),
                            ],
                        ),
                    ),
                );
            }
            if let Some(series) = &item.series_name {
                items.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Series", key_style), Span::raw(": "),
                                Span::raw(series.clone()),
                            ],
                        ),
                    ),
                );
            }
            if let Some(year) = item.year {
                items.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Year", key_style), Span::raw(": "),
                                Span::raw(year.to_string()),
                            ],
                        ),
                    ),
                );
            }
            items.push(
                ListItem::new(
                    Line::from(
                        vec![
                            Span::styled("Type", key_style), Span::raw(": "),
                            Span::raw(item.kind.clone()),
                        ],
                    ),
                ),
            );
        } else {
            items.push(ListItem::new(
                Line::styled("Type a search term to find items across your libraries", ctx.config.as_list_text_style()),
            ));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        let inner = block.inner(area);
        let list = List::new(items).style(ctx.config.as_list_name_style());
        ratatui::widgets::StatefulWidget::render(
            list,
            inner,
            frame.buffer_mut(),
            &mut self.info_state,
        );
        frame.render_widget(block, area);
    }


}
impl Pane for JellyfinPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        if area.height < 2 {
            return Ok(());
        }
        let [toggle_area, content] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(area);
        self.render_toggle(frame, toggle_area, ctx);
        match self.mode {
            JellyfinTabMode::Libraries => {
                self.render_tree_browser(frame, content, ctx)
            }
            JellyfinTabMode::Search => self.render_search(frame, content, ctx),
        }
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
        let MpdQueryResult::Any(any) = data else { return Ok(()) };
        match any.downcast::<crate::jellyfin::JellyfinResult>() {
            Ok(boxed) => {
                // Round 92: a tracked library fetch finished (result or
                // error alike), so its in-flight slot goes away — the last
                // one clears the loading indicator.
                if Self::is_tracked_fetch(id) {
                    self.end_fetch(ctx);
                }
                match (*boxed, id) {
                    (crate::jellyfin::JellyfinResult::Error(_), JF_IMAGE) => {
                        self.poster.clear(ctx);
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Chapters { item_id, chapters },
                        JF_CHAPTERS,
                    ) => {
                        if ctx.mpv.active
                            && ctx.mpv.item_id.as_deref() == Some(item_id.as_str())
                        {
                            ctx.chapters.borrow_mut().insert(item_id, chapters);
                        } else if let Some((_, song)) = ctx.find_current_song_in_queue()
                            && crate::jellyfin::item_id_from_url(&song.file).as_deref()
                                == Some(&item_id)
                        {
                            ctx.chapters
                                .borrow_mut()
                                .insert(song.file.clone(), chapters);
                            ctx.auto_show_chapters();
                        }
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Image { item_id, bytes },
                        JF_IMAGE,
                    ) => {
                        if self.poster.item_id.as_deref() == Some(item_id.as_str()) {
                            self.poster.set_bytes(item_id, bytes);
                        }
                        ctx.render()?;
                    }
                    (crate::jellyfin::JellyfinResult::Item(item), JF_ITEM) => {
                        self.full_item = Some(item);
                        ctx.render()?;
                    }
                    (crate::jellyfin::JellyfinResult::Error(err), _) => {
                        if id == JF_SEARCH {
                            self.search_pending = false;
                        }
                        self.error = Some(err.clone());
                        status_warn!("Jellyfin: {err}");
                        ctx.render()?;
                    }
                    (crate::jellyfin::JellyfinResult::SearchHints { items }, JF_SEARCH) => {
                        self.search_pending = false;
                        self.search_results = items;
                        // Typed-ahead queries are re-sent until the
                        // results match the current input.
                        let current = ctx.input.value(self.search_buffer).trim().to_owned();
                        if current != self.last_sent_query && !current.is_empty() {
                            self.run_search(ctx);
                        }
                        if let Some(first) = self.search_results.first().cloned() {
                            self.search_results_state.select(Some(0));
                            let _ = first;
                        } else {
                            self.search_results_state = ListState::default();
                        }
                        ctx.render()?;
                    }
                    (crate::jellyfin::JellyfinResult::Views(views), JF_VIEWS) => {
                        self.views = views;
                        self.error = None;
                        // Round 92: remember the fresh list for the next
                        // start (the first frame paints from it). A failed
                        // refresh never reaches here, so the cache is only
                        // ever replaced by server data.
                        if let Some(server) = self.server.as_ref() {
                            crate::jellyfin::save_views_cache(
                                ctx.config.cache_dir.as_deref(),
                                &server.base,
                                &server.user_id,
                                &self.views,
                            );
                        }
                        self.rebuild_tree();
                        if self.selected.is_none() {
                            if !self.tree.is_empty() {
                                self.tree_list.select(Some(0));
                            }
                            self.populate_items();
                            self.sync_poster(ctx);
                        } else {
                            self.populate_items();
                        }
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Children { parent_id, items },
                        JF_FOLDER,
                    ) => {
                        self.folders.insert(parent_id, items);
                        self.rebuild_tree();
                        self.populate_items();
                        self.sync_tree_to_items_cursor();
                        self.sync_poster(ctx);
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Artists { view_id, items },
                        JF_ARTISTS,
                    ) => {
                        self.artists.insert(view_id, items);
                        self.rebuild_tree();
                        self.populate_items();
                        self.sync_tree_to_items_cursor();
                        self.sync_poster(ctx);
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Albums { artist_id, items },
                        JF_ALBUMS,
                    ) => {
                        self.albums.insert(artist_id, items);
                        self.rebuild_tree();
                        self.populate_items();
                        self.sync_tree_to_items_cursor();
                        self.sync_poster(ctx);
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Songs { album_id, items },
                        JF_SONGS,
                    ) => {
                        self.songs.insert(album_id, items);
                        self.populate_items();
                        self.sync_tree_to_items_cursor();
                        self.sync_poster(ctx);
                        ctx.render()?;
                    }
                    _ => {}
                }
            }
            Err(any) => {
                if id == JF_PLAY {
                    self.handle_play_result(any, ctx)?;
                }
            }
        }
        Ok(())
    }
}
/// Play a Jellyfin stream URL as a temporary (queue-free) MPD entry. Used by
/// the Ask menu's "Play audio with MPD" option, where the pane itself cannot
/// be borrowed (the menu stores the closure for later).
fn jellyfin_play_temp(ctx: &Ctx, url: String) {
    ctx.query()
        .id(JF_PLAY)
        .replace_id(JF_PLAY)
        .target(PaneType::Jellyfin {
            tree: TreeBrowserArgs::default(),
        })
        .query(move |client| {
            let id = client.add_id(&url, None)?;
            client.play_id(id)?;
            Ok(MpdQueryResult::Any(Box::new(id)))
        });
}
/// Poster / episode preview of the selected item, drawn as a terminal-side
/// overlay using the same resolved image backend as the album art (kitty /
/// sixel / iterm2 / ueberzug / block), so the terminal's best image
/// protocol is used. The overlay persists between frames (ratatui diffs
/// don't overwrite it); it is cleared on selection change and re-drawn when
/// the new image arrives or the area changes.
#[derive(Debug)]
struct JfPoster {
    backend: PosterBackend,
    /// Primary-image bytes of the selected item.
    bytes: Option<std::sync::Arc<Vec<u8>>>,
    /// Item whose poster is loaded/requested (for stale-result checks).
    item_id: Option<String>,
    /// When the item has no own image, the id whose image is used instead
    /// (e.g. the series poster for a season without art); re-used by a
    /// failed-bytes redraw.
    fallback_id: Option<String>,
    /// Pixel dimensions of the loaded artwork (header-only decode), used to
    /// size the shelf to the image's aspect (round 66).
    img_px: Option<(u32, u32)>,
    /// `album_art.max_size_px` captured at construction, reused to compute
    /// the artwork's display width at the shelf's fixed height.
    max_size_px: crate::config::Size,
    /// Area where the overlay was last drawn (None = nothing drawn).
    drawn_area: Option<Rect>,
    /// Encoded image queued during render but not yet displayed: the
    /// overlay must go up *after* the frame's buffer flush, which would
    /// otherwise overwrite the kitty placeholder cells (they changed while
    /// other tabs were shown).
    pending: Option<EncodeData>,
    pending_area: Option<Rect>,
}
#[derive(Debug)]
enum PosterBackend {
    Kitty(Kitty),
    Ueberzug(Ueberzug),
    Iterm2(Iterm2),
    Sixel(Sixel),
    Block(ImageBlock),
    None,
}
impl JfPoster {
    fn new(ctx: &Ctx) -> Self {
        let backend = match ctx.config.album_art.method {
            crate::config::album_art::ImageMethod::Kitty => PosterBackend::Kitty(Kitty),
            crate::config::album_art::ImageMethod::UeberzugWayland => {
                PosterBackend::Ueberzug(Ueberzug::new(Layer::Wayland))
            }
            crate::config::album_art::ImageMethod::UeberzugX11 => {
                PosterBackend::Ueberzug(Ueberzug::new(Layer::X11))
            }
            crate::config::album_art::ImageMethod::Iterm2 => {
                PosterBackend::Iterm2(Iterm2)
            }
            crate::config::album_art::ImageMethod::Sixel => PosterBackend::Sixel(Sixel),
            crate::config::album_art::ImageMethod::Block => {
                PosterBackend::Block(ImageBlock)
            }
            crate::config::album_art::ImageMethod::None => PosterBackend::None,
        };
        Self {
            backend,
            bytes: None,
            item_id: None,
            fallback_id: None,
            img_px: None,
            max_size_px: ctx.config.album_art.max_size_px,
            drawn_area: None,
            pending: None,
            pending_area: None,
        }
    }
    /// Clear the current poster: hide the overlay and drop the bytes (used
    /// when the selection changes; the new item's image is drawn on
    /// arrival).
    fn clear(&mut self, ctx: &Ctx) {
        if let Some(area) = self.drawn_area.take() {
            self.hide_at(area, ctx);
        }
        self.pending = None;
        self.pending_area = None;
        self.bytes = None;
        self.item_id = None;
        self.img_px = None;
    }
    fn hide_at(&mut self, area: Rect, ctx: &Ctx) {
        let bg = ctx.config.theme.background_color.map(|c| c.into_crossterm());
        let writer = crate::shared::terminal::TERMINAL.writer();
        let mut w = writer.lock();
        let w = w.by_ref();
        match &mut self.backend {
            PosterBackend::Kitty(b) => {
                let _ = b.hide(w, area, bg);
            }
            PosterBackend::Ueberzug(b) => {
                let _ = b.hide(w, area, bg);
            }
            PosterBackend::Iterm2(b) => {
                let _ = b.hide(w, area, bg);
            }
            PosterBackend::Sixel(b) => {
                let _ = b.hide(w, area, bg);
            }
            PosterBackend::Block(b) => {
                let _ = b.hide(w, area, bg);
            }
            PosterBackend::None => {}
        }
    }
    fn set_bytes(&mut self, item_id: String, bytes: Vec<u8>) {
        self.item_id = Some(item_id);
        self.img_px = Self::image_dimensions(&bytes);
        self.bytes = Some(std::sync::Arc::new(bytes));
        self.drawn_area = None;
    }
    /// Header-only decode of the artwork's pixel size (no full decode —
    /// cheap enough to run when the bytes arrive). `None` on any failure.
    fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
        use std::io::Cursor;
        image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()
    }
    /// The artwork's width in terminal cells when displayed at `inner_h`
    /// rows tall (the shelf's fixed inner height) — the shelf adds 2 border
    /// columns plus a 1-char margin on each side (round 66). Mirrors
    /// `create_aligned_area`'s height-limited fit for an unbounded-width
    /// area, so the encoder fills the resulting inner area exactly. Falls
    /// back to `POSTER_SHELF_W` when the image isn't loaded yet or the
    /// terminal pixel metrics are unavailable.
    fn natural_width_cells(&self, inner_h: u16) -> u16 {
        let Some((iw, ih)) = self.img_px else { return POSTER_SHELF_W };
        if iw == 0 || ih == 0 {
            return POSTER_SHELF_W;
        }
        let Ok(ws) = crossterm::terminal::window_size() else {
            return POSTER_SHELF_W;
        };
        if ws.width == 0 || ws.height == 0 || ws.rows == 0 || ws.columns == 0 {
            return POSTER_SHELF_W;
        }
        let cell_w = ws.width as f64 / ws.columns as f64;
        let cell_h = ws.height as f64 / ws.rows as f64;
        let bounds_w = self.max_size_px.width as f64;
        let bounds_h = ((inner_h as f64) * cell_h).min(self.max_size_px.height as f64);
        let scale = (bounds_w / iw as f64).min(bounds_h / ih as f64);
        let used_w_px = iw as f64 * scale;
        ((used_w_px / cell_w).ceil() as u16).max(1)
    }
    /// Hide the overlay (tab switch, modal opened).
    fn hide(&mut self, ctx: &Ctx) {
        if let Some(area) = self.drawn_area.take() {
            self.hide_at(area, ctx);
        }
        self.pending = None;
        self.pending_area = None;
    }
    fn encode(&mut self, area: Rect, ctx: &Ctx) -> Option<EncodeData> {
        let bytes = self.bytes.as_ref()?;
        let max_size = ctx.config.album_art.max_size_px;
        let halign = ctx.config.album_art.horizontal_align;
        let valign = ctx.config.album_art.vertical_align;
        let result = match &self.backend {
            PosterBackend::Kitty(_) => {
                Kitty::create_data(bytes, area, max_size, halign, valign)
                    .map(EncodeData::Kitty)
            }
            PosterBackend::Ueberzug(_) => {
                Ueberzug::create_data(bytes, area, max_size, halign, valign)
                    .map(EncodeData::Ueberzug)
            }
            PosterBackend::Iterm2(_) => {
                Iterm2::create_data(bytes, area, max_size, halign, valign)
                    .map(EncodeData::Iterm2)
            }
            PosterBackend::Sixel(_) => {
                Sixel::create_data(bytes, area, max_size, halign, valign)
                    .map(EncodeData::Sixel)
            }
            PosterBackend::Block(_) => {
                ImageBlock::create_data(bytes, area, max_size, halign, valign)
                    .map(EncodeData::Block)
            }
            PosterBackend::None => return None,
        };
        match result {
            Ok(data) => Some(data),
            Err(err) => {
                log::debug!(error:? = err; "Failed to encode jellyfin poster");
                None
            }
        }
    }
    fn display(&mut self, data: EncodeData, ctx: &Ctx) -> bool {
        let writer = crate::shared::terminal::TERMINAL.writer();
        let mut w = writer.lock();
        let w = w.by_ref();
        match (&mut self.backend, data) {
            (PosterBackend::Kitty(b), EncodeData::Kitty(d)) => {
                b.display(w, d, ctx).is_ok()
            }
            (PosterBackend::Ueberzug(b), EncodeData::Ueberzug(d)) => {
                b.display(w, d, ctx).is_ok()
            }
            (PosterBackend::Iterm2(b), EncodeData::Iterm2(d)) => {
                b.display(w, d, ctx).is_ok()
            }
            (PosterBackend::Sixel(b), EncodeData::Sixel(d)) => {
                b.display(w, d, ctx).is_ok()
            }
            (PosterBackend::Block(b), EncodeData::Block(d)) => {
                b.display(w, d, ctx).is_ok()
            }
            _ => false,
        }
    }
    /// Draw the poster into `area`, re-encoding only when the area changed
    /// since the last draw. The display itself is deferred to
    /// `flush_pending`, which the event loop calls *after* the frame's
    /// buffer flush — displaying during the render would let that flush
    /// overwrite the kitty placeholder cells with the newly drawn tab
    /// content (the cells changed while other tabs were shown).
    fn draw(&mut self, area: Rect, ctx: &Ctx) {
        let show = self.bytes.is_some() && area.height >= 3 && area.width >= 3;
        if !show {
            self.pending = None;
            self.pending_area = None;
            if let Some(old) = self.drawn_area.take() {
                self.hide_at(old, ctx);
            }
            return;
        }
        if self.drawn_area == Some(area) {
            return;
        }
        if self.pending.is_some() && self.pending_area == Some(area) {
            return;
        }
        let Some(data) = self.encode(area, ctx) else {
            self.drawn_area = Some(area);
            return;
        };
        self.pending = Some(data);
        self.pending_area = Some(area);
    }
    /// Display the queued overlay (called after the frame flush). No-op
    /// when nothing is queued.
    fn flush_pending(&mut self, ctx: &Ctx) {
        let Some(data) = self.pending.take() else { return };
        let area = self.pending_area.take().unwrap_or_default();
        if let Some(old) = self.drawn_area.take() && old != area {
            self.hide_at(old, ctx);
        }
        if self.display(data, ctx) {
            self.drawn_area = Some(area);
        }
    }
    /// Force the next render to redraw the overlay (the tab was reopened
    /// and the overlay was hidden on the way out). Re-requests the image
    /// when the bytes are missing (an earlier fetch failed).
    fn redraw_next_render(&mut self, ctx: &Ctx) {
        self.drawn_area = None;
        if self.bytes.is_none() && let Some(item_id) = self.item_id.clone() {
            let _ = ctx
                .work_sender
                .send(WorkRequest::FetchJellyfinImage {
                    item_id,
                    fallback_item_id: self.fallback_id.clone(),
                })
                .map_err(|err| {
                    log::error!(error:? = err; "Failed to request jellyfin poster")
                });
        }
    }
}

use std::{
    collections::{HashMap, HashSet},
    io::Write,
};
use anyhow::Result;
use ratatui::{
    Frame, layout::{Alignment, Constraint, Layout, Rect},
    prelude::IntoCrossterm, style::{Color, Modifier},
    text::{Line, Span},
    widgets::{Borders, List, ListItem, ListState, Paragraph},
};
use super::Pane;
use crate::{
    MpdQueryResult, config::tabs::{PaneType, PaneTypeDiscriminants, TreeBrowserArgs},
    ctx::Ctx, jellyfin::{Jellyfin, JfItem},
    mpd::{commands::State, mpd_client::MpdClient},
    shared::{
        events::WorkRequest, keys::ActionEvent,
        macros::{modal, status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{
        UiEvent,
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
const JF_PLAY: &str = "jellyfin_play";
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
    tree_area: Rect,
    items_area: Rect,
    info_area: Rect,
    /// Poster / episode preview of the selected item (terminal-side
    /// overlay, best image protocol for the terminal).
    poster: JfPoster,
    /// Whether a modal (settings panel, popup, ...) is open on top of the
    /// tab; the poster overlay is not drawn while one is up.
    is_modal_open: bool,
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
            tree_area: Rect::default(),
            items_area: Rect::default(),
            info_area: Rect::default(),
            poster: JfPoster::new(ctx),
            is_modal_open: false,
            info_state: ListState::default(),
            info_items_len: 0,
            info_song_id: None,
            full_item: None,
            info_song_shown_at: None,
            info_scrollbar_area: Rect::default(),
            info_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            item_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
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
        let _ = ctx
            .work_sender
            .send(WorkRequest::FetchJellyfinViews)
            .map_err(|err| {
                log::error!(error:? = err; "Failed to request jellyfin views")
            });
    }
    /// Lazy-load the children of a node kind.
    fn ensure_loaded(&mut self, kind: &JfNodeKind, ctx: &Ctx) {
        if self.load_server(ctx).is_none() {
            return;
        }
        let send = |ctx: &Ctx, request: WorkRequest| {
            let _ = ctx
                .work_sender
                .send(request)
                .map_err(|err| {
                    log::error!(error:? = err; "Failed to request jellyfin data")
                });
        };
        match kind {
            JfNodeKind::View(item) => {
                if item.is_music_view {
                    if !self.artists.contains_key(&item.id) {
                        send(
                            ctx,
                            WorkRequest::FetchJellyfinArtists {
                                view_id: item.id.clone(),
                            },
                        );
                    }
                } else if !self.folders.contains_key(&item.id) {
                    send(
                        ctx,
                        WorkRequest::FetchJellyfinFolder {
                            parent_id: item.id.clone(),
                        },
                    );
                }
            }
            JfNodeKind::Artist(item) => {
                if !self.albums.contains_key(&item.id) {
                    send(
                        ctx,
                        WorkRequest::FetchJellyfinAlbums {
                            artist_id: item.id.clone(),
                        },
                    );
                }
            }
            JfNodeKind::Album(item) => {
                if !self.songs.contains_key(&item.id) {
                    send(
                        ctx,
                        WorkRequest::FetchJellyfinSongs {
                            album_id: item.id.clone(),
                        },
                    );
                }
            }
            JfNodeKind::Folder(item) => {
                if !self.folders.contains_key(&item.id) {
                    send(
                        ctx,
                        WorkRequest::FetchJellyfinFolder {
                            parent_id: item.id.clone(),
                        },
                    );
                }
            }
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
            move |ctx: &Ctx| {
                if let Some(season_id) = season_id.clone() {
                    let _ = ctx
                        .work_sender
                        .send(crate::shared::events::WorkRequest::FetchJellyfinSeason {
                            season_id,
                            episode_id: item_id.clone(),
                        });
                } else {
                    crate::core::mpv::run_mpv(ctx, &url);
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
                    let text_width = (((area.width.saturating_sub(2)) * 3 / 5)
                        .saturating_sub(3))
                        .max(10) as usize;
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
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        let inner = block.inner(area);
        let image_only = selected
            .as_ref()
            .is_some_and(|item| {
                matches!(item.kind.as_str(), "CollectionFolder" | "Season")
            })
            || (selected.is_none()
                && self
                    .selected
                    .as_ref()
                    .is_some_and(|k| {
                        matches!(k.item().kind.as_str(), "CollectionFolder" | "Season")
                    }));
        if image_only {
            if !self.is_modal_open {
                self.poster.draw(inner, ctx);
            }
            frame.render_widget(block, area);
            self.info_area = Rect::default();
            self.info_scrollbar_area = Rect::default();
            return;
        }
        let [poster_area, text_area] = Layout::horizontal([
                Constraint::Percentage(40),
                Constraint::Percentage(60),
            ])
            .areas(inner);
        if !self.is_modal_open {
            self.poster.draw(poster_area, ctx);
        }
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
            let [a, b] = Layout::horizontal([
                    Constraint::Percentage(100),
                    Constraint::Length(1),
                ])
                .areas(body_area);
            (a, b)
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
            ratatui::widgets::StatefulWidget::render(
                scrollbar,
                scrollbar_area,
                frame.buffer_mut(),
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
    fn tips_lines(&self, ctx: &Ctx) -> Vec<Line<'static>> {
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        vec![
            Line::from(vec![Span::styled("w/s · ↑/↓", base),
            Span::styled("  libraries · items", dim),]),
            Line::from(vec![Span::styled("d / a", base),
            Span::styled("  expand · collapse", dim)]),
            Line::from(vec![Span::styled("Enter · →", base),
            Span::styled("  play track · open", dim),]),
        ]
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
impl Pane for JellyfinPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        self.render_tree_browser(frame, area, ctx)
    }
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.initialized || self.error.is_some() {
            self.fetch_views(ctx);
        }
        self.initialized = true;
        self.poster.redraw_next_render(ctx);
        Ok(())
    }
    fn on_hide(&mut self, ctx: &Ctx) -> Result<()> {
        self.poster.hide(ctx);
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
        if self.tree_area.contains(event.into()) {
            return self.handle_tree_mouse(event, ctx);
        }
        if self.info_scrollbar_area.height > 0
            && matches!(
                event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. }
            ) && self.info_scrollbar_area.contains(event.into())
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
        self.handle_tree_action(event, ctx)?;
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
                        self.error = Some(err.clone());
                        status_warn!("Jellyfin: {err}");
                        ctx.render()?;
                    }
                    (crate::jellyfin::JellyfinResult::Views(views), JF_VIEWS) => {
                        self.views = views;
                        self.error = None;
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
        self.bytes = Some(std::sync::Arc::new(bytes));
        self.drawn_area = None;
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

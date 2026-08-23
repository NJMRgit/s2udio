use std::collections::{HashMap, HashSet};
use anyhow::Result;
use itertools::Itertools;
use ratatui::{
    Frame, layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};
use super::{Pane, search::SearchPane};
use crate::{
    MpdQueryResult,
    config::{
        keys::GlobalAction, tabs::{PaneType, PaneTypeDiscriminants, TreeBrowserArgs},
    },
    ctx::Ctx,
    mpd::{
        client::Client, commands::Song, mpd_client::{Filter, FilterKind, MpdClient, Tag},
    },
    shared::{
        keys::ActionEvent, macros::modal, mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::{Enqueue, MpdClientExt},
    },
    ui::{
        UiEvent, dir_or_song::DirOrSong, dirstack::{DirStackItem, MarkState, Path},
        input::InputResultEvent,
        modals::{
            input_modal::InputModal, menu::modal::MenuModal, select_modal::SelectModal,
        },
        tree_browser::{TreeBrowserCore, TreeRowView},
        widgets::sub_tab_bar::{Segment, SubTabBar},
    },
};
const FETCH_DATA: &str = "fetch_data";
const TREE: &str = "dir_tree";
const PLAY_FILE: &str = "dir_play_file";
/// Width of the left folder-tree pane for a given total width. The tree
/// keeps a minimum of 50 columns when there is room; on TUIs 120 columns
/// wide or less it is hidden entirely (the right pane takes the whole
/// area). The tree never takes more than `total - 1`, so the right pane
/// always keeps at least one column.
///
/// Shared by the MPD (directories), Playlists and Jellyfin tabs: all
/// three browser panes split their width with this helper.
/// Test-only parity pin for the round-7/8 tree-width behavior (the
/// production callers read the args directly via
/// `TreeBrowserArgs::tree_width`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn tree_width(total: u16) -> u16 {
    TreeBrowserArgs::default().tree_width(total)
}
/// Split a full MPD path ("A/B/C") into a dirstack `Path`.
fn split_path(full: &str) -> Path {
    let mut path = Path::new();
    for seg in full.split('/') {
        path.push(seg.to_owned());
    }
    path
}
/// A browser entry is shown when it is not a playlist and not a hidden
/// directory (name starting with `.`, e.g. `.hist`).
fn is_visible_entry(v: &DirOrSong) -> bool {
    match v {
        DirOrSong::Dir { name, playlist, .. } => !*playlist && !name.starts_with('.'),
        DirOrSong::Song(_) => true,
    }
}
/// Whether a browser path is the Downloads folder (the marker segment,
/// `~/Downloads/s2udio-downloads` shown as "Downloads").
fn is_downloads_path(path: &Path) -> bool {
    path.as_slice().len() == 1
        && path.as_slice()[0] == crate::ui::modals::paste::DOWNLOADS_DIR_NAME
}
/// List the downloads folder from disk: it lives outside the MPD library,
/// so MPD cannot list it. Files carry absolute paths (playable via mpv)
/// and their file stem as the title (no tags on disk files); subfolders
/// and dotfiles are skipped. Empty when the folder is missing/unreadable.
fn list_downloads_dir() -> Vec<DirOrSong> {
    let Some(dir) = crate::ui::modals::paste::downloads_dir() else {
        return Vec::new();
    };
    list_dir(&dir)
}
/// The actual disk listing behind [`list_downloads_dir`] (separated for
/// tests — no `$HOME` dependence).
fn list_dir(dir: &std::path::Path) -> Vec<DirOrSong> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut items: Vec<DirOrSong> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = std::path::Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.clone());
            let mut metadata = std::collections::HashMap::new();
            metadata
                .insert(
                    "title".to_owned(),
                    crate::mpd::commands::metadata_tag::MetadataTag::from(stem),
                );
            DirOrSong::Song(crate::mpd::commands::Song {
                file: path.to_string_lossy().into_owned(),
                duration: None,
                metadata,
                last_modified: e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| chrono::DateTime::from_timestamp(d.as_secs() as i64, 0))
                    .flatten()
                    .unwrap_or(chrono::Utc::now()),
                ..Default::default()
            })
        })
        .collect();
    items.sort_by(|a, b| a.as_path().cmp(b.as_path()));
    items
}
/// A collapsible folder tree for the left pane, built from `listall`.
#[derive(Debug)]
struct TreeNode {
    name: String,
    path: Vec<String>,
    /// Overrides the rendered name (the downloads folder shows
    /// as "Downloads").
    display: Option<String>,
    children: Vec<TreeNode>,
    expanded: bool,
}
impl TreeNode {
    fn new(name: String, path: Vec<String>) -> Self {
        Self {
            name,
            path,
            display: None,
            children: Vec::new(),
            expanded: false,
        }
    }
    fn find_mut(&mut self, path: &[String]) -> Option<&mut TreeNode> {
        let mut node = self;
        for seg in path {
            let i = node.children.iter().position(|c| &c.name == seg)?;
            node = &mut node.children[i];
        }
        Some(node)
    }
    /// The name the tree renders for a node (the display override for the
    /// Downloads folder, the path segment otherwise).
    fn display_name(&self) -> &str {
        self.display.as_deref().unwrap_or(&self.name)
    }
}
#[derive(Debug)]
pub struct DirTree {
    root: TreeNode,
    selected: usize,
}
impl Default for DirTree {
    fn default() -> Self {
        Self {
            root: TreeNode {
                name: "Library".to_owned(),
                path: Vec::new(),
                display: None,
                children: Vec::new(),
                expanded: true,
            },
            selected: 0,
        }
    }
}
impl DirTree {
    /// Build the tree from `listall` directory paths (e.g. "Artist/Album").
    /// Hidden directories (a segment starting with `.`) are skipped
    /// entirely, along with everything under them.
    fn build(dirs: impl Iterator<Item = String>) -> Self {
        let mut tree = Self::default();
        for full_path in dirs {
            if full_path.split('/').any(|seg| seg.starts_with('.')) {
                continue;
            }
            let mut node = &mut tree.root;
            let mut path = Vec::new();
            for seg in full_path.split('/') {
                path.push(seg.to_owned());
                let idx = node.children.iter().position(|c| c.name == seg);
                node = if let Some(i) = idx {
                    &mut node.children[i]
                } else {
                    node.children.push(TreeNode::new(seg.to_owned(), path.clone()));
                    node.children.last_mut().expect("just pushed")
                };
            }
        }
        tree
    }
    /// Flatten the visible nodes (pre-order, skipping collapsed subtrees)
    /// into (node, depth) pairs.
    fn visible<'a>(
        &'a self,
        node: &'a TreeNode,
        depth: usize,
        out: &mut Vec<(&'a TreeNode, usize)>,
    ) {
        out.push((node, depth));
        if node.expanded {
            for child in &node.children {
                self.visible(child, depth + 1, out);
            }
        }
    }
    /// Expand `path` (and its ancestors) and select its node. Only an
    /// explicit open action expands the target; the root is always expanded
    /// so the tree can never fully collapse.
    fn sync(&mut self, path: &Path) {
        self.root.expanded = true;
        let segments = path.as_slice();
        let mut node = &mut self.root;
        for seg in segments {
            let Some(i) = node.children.iter().position(|c| &c.name == seg) else {
                break;
            };
            let child = &mut node.children[i];
            child.expanded = true;
            node = child;
        }
        let mut flat = Vec::new();
        self.visible(&self.root, 0, &mut flat);
        self.selected = flat
            .iter()
            .position(|(n, _)| n.path == path.as_slice())
            .unwrap_or(0);
    }
    fn expand(&mut self, path: &[String]) {
        if let Some(node) = self.root.find_mut(path) {
            node.expanded = true;
        }
    }
    fn collapse(&mut self, path: &[String]) {
        if let Some(node) = self.root.find_mut(path) {
            node.expanded = false;
        }
    }
}
/// The mode of the MPD tab (round 28): the tab hosts the library browser
/// (folders + items) and the search UI (the former top-level Search tab)
/// under one toggle. The active mode is marked with the app's ●/⭘
/// convention (`⭘ Library  ● Search`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpdTabMode {
    /// The folder tree + items browser (the pre-round-28 MPD tab).
    Library,
    /// The search filters + results UI (the folded-in Search tab).
    Search,
}
/// The MPD tab: a jellyfin-style shared-selection browser. The left tree
/// mirrors the folder hierarchy (the `Library ↴` root, always expanded),
/// the right pane lists the **current node's children** — folders and
/// songs one level deep; at the root it lists every top-level directory.
///
/// Highlighting never expands anything and never lists files recursively:
/// `d`/`→` open a folder (expand its tree path, show its children)
/// or play a file, `a`/`←` back out one level and collapse the branch
/// left, `Enter` opens the context menu (like right-click, parity with
/// the Playlists pane), and the tree highlight follows the right-pane
/// cursor.
///
/// Round 28: the tab also hosts the search UI — a `⭘ Library  ● Search`
/// toggle row at the top switches between the library browser and the
/// folded-in search mode (the former top-level Search tab; still queries
/// the user's MPD library). The search state lives for the session, so
/// filters and results survive Library↔Search toggles; the mode resets to
/// Library at startup.
#[derive(Debug)]
pub struct DirectoriesPane {
    /// Folder tree (Library root) built from `listall`.
    tree: DirTree,
    tree_state: ListState,
    tree_inner: Rect,
    /// Children (folders + songs) of the current node; at the root the
    /// top-level directories.
    items: Vec<DirOrSong>,
    item_list: ListState,
    items_inner: Rect,
    /// Marked (multi-selected) rows of the right pane, with the queue
    /// tab's ctrl/alt-click + shift+up/down selection.
    marked: MarkState,
    /// Path of the node whose children are shown (None = the Library root).
    selected: Option<Path>,
    /// Tree-browser layout args from the config (defaults = today's
    /// constants: 50-col minimum tree, hidden <= 120, info cap 15).
    tree_args: TreeBrowserArgs,
    /// Children lists keyed by path, so backing out never refetches.
    loaded: HashMap<Path, Vec<DirOrSong>>,
    /// Paths with a fetch in flight (avoid duplicate requests).
    pending: HashSet<Path>,
    /// Queue id of a file played via `PlayFile` (`d`/`→`); dropped on song
    /// change / stop — mirrors the Radio pane.
    temp_play_id: Option<u32>,
    initialized: bool,
    /// The active mode (Library or the folded-in Search). Reset to Library
    /// at startup, kept for the session (round 28 defaults).
    mode: MpdTabMode,
    /// The search UI, alive for the session so filters/results survive
    /// Library↔Search toggles.
    search: SearchPane,
    /// Click areas of the toggle row's two labels (Library, Search),
    /// refreshed on every render.
    toggle_areas: [Rect; 2],
}
impl DirectoriesPane {
    pub fn new(ctx: &Ctx) -> Self {
        Self {
            tree: DirTree::default(),
            tree_state: ListState::default(),
            tree_inner: Rect::default(),
            items: Vec::new(),
            item_list: ListState::default(),
            items_inner: Rect::default(),
            marked: MarkState::default(),
            selected: None,
            tree_args: ctx.config.tree_browser_args(PaneTypeDiscriminants::Directories),
            loaded: HashMap::new(),
            pending: HashSet::new(),
            temp_play_id: None,
            initialized: false,
            mode: MpdTabMode::Library,
            search: SearchPane::new(ctx),
            toggle_areas: [Rect::default(); 2],
        }
    }
    /// Switch the tab's mode (Library <-> Search), keeping the search
    /// state alive for the session. No-op when already in the mode.
    pub fn set_mode(&mut self, mode: MpdTabMode, ctx: &Ctx) -> Result<()> {
        if self.mode == mode {
            return Ok(());
        }
        self.mode = mode;
        ctx.render()?;
        Ok(())
    }
    /// Flip the Library/Search mode (bound to `Tab` while the MPD tab is
    /// focused and to clicking the toggle labels).
    pub fn toggle_mode(&mut self, ctx: &Ctx) -> Result<()> {
        let next = match self.mode {
            MpdTabMode::Library => MpdTabMode::Search,
            MpdTabMode::Search => MpdTabMode::Library,
        };
        self.set_mode(next, ctx)
    }
    /// The `⭘ Library  ● Search` toggle row: left-aligned, one leading
    /// space, the ●/⭘ marker convention with the active mode bold. Click
    /// areas for the two labels are recorded for mouse routing; the row
    /// renders in both modes (round 28).
    fn render_toggle(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        self.toggle_areas = [Rect::default(); 2];
        if area.height == 0 {
            return;
        }
        let segments = [
            Segment {
                label: "Library",
                active: self.mode == MpdTabMode::Library,
            },
            Segment {
                label: "Search",
                active: self.mode == MpdTabMode::Search,
            },
        ];
        let x = area.x.saturating_add(1);
        let bar = SubTabBar::new(&segments, x, area.y, area.right().saturating_sub(1));
        let areas = bar.render(frame, ctx);
        for (idx, seg_area) in areas.into_iter().take(2).enumerate() {
            self.toggle_areas[idx] = seg_area;
        }
    }
    /// Fetch the children of `path` ("" = root): the folders + songs shown
    /// in the right pane. Playlists are handled by the Playlists tab and
    /// hidden directories are never listed; the MPD browser shows folders
    /// and songs only. The Downloads folder is the exception: it lives
    /// outside the MPD library, so its listing comes from disk instead of
    /// MPD `lsinfo` (and is never cached — downloads appear/disappear).
    fn fetch_children(&mut self, path: &Path, ctx: &Ctx) {
        if self.pending.contains(path) || self.loaded.contains_key(path) {
            return;
        }
        if is_downloads_path(path) {
            self.loaded.insert(path.clone(), list_downloads_dir());
            return;
        }
        self.pending.insert(path.clone());
        let path = path.clone();
        let sort = ctx.config.directories_sort.clone();
        let playlist_display_mode = ctx.config.show_playlists_in_browser;
        ctx.query()
            .id(FETCH_DATA)
            .replace_id("directories_data")
            .target(PaneType::Directories {
                tree: TreeBrowserArgs::default(),
            })
            .query(move |client| {
                let entries = if path.is_empty() {
                    client.lsinfo(None)?
                } else {
                    client.lsinfo(Some(&path.to_string()))?
                };
                let data: Vec<DirOrSong> = entries
                    .0
                    .into_iter()
                    .filter_map(|v| v.into_dir_or_song(playlist_display_mode))
                    .filter(is_visible_entry)
                    .sorted_by(|a, b| {
                        a.with_custom_sort(&sort).cmp(&b.with_custom_sort(&sort))
                    })
                    .collect();
                Ok(MpdQueryResult::DirOrSong {
                    data,
                    path: Some(path),
                })
            });
    }
    /// Populate the right pane for the current node: always its children
    /// (folders + songs one level deep). At the root (`selected` = None)
    /// the right pane lists every top-level directory. Playlists and
    /// hidden directories never display here.
    fn populate_items(&mut self) {
        self.item_list.select(None);
        self.marked.clear();
        let mut items: Vec<DirOrSong> = match self.selected.as_ref() {
            Some(path) => self.loaded.get(path).cloned().unwrap_or_default(),
            None => self.loaded.get(&Path::new()).cloned().unwrap_or_default(),
        }
            .into_iter()
            .filter(is_visible_entry)
            .collect();
        if self.selected.is_none() {
            items
                .insert(
                    0,
                    DirOrSong::Dir {
                        name: "Downloads".to_owned(),
                        full_path: crate::ui::modals::paste::DOWNLOADS_DIR_NAME
                            .to_owned(),
                        last_modified: chrono::Utc::now(),
                        playlist: false,
                    },
                );
        }
        self.items = items;
        if !self.items.is_empty() {
            self.item_list.select(Some(0));
            *self.item_list.offset_mut() = 0;
        }
    }
    /// Highlight `path` in the right pane (the row we came from when
    /// backing out), falling back to the first row.
    fn select_items_item(&mut self, path: &Path) {
        let target = path.to_string();
        let idx = self
            .items
            .iter()
            .position(|item| match item {
                DirOrSong::Dir { full_path, .. } => full_path == &target,
                DirOrSong::Song(_) => false,
            })
            .unwrap_or(0);
        if !self.items.is_empty() {
            self.item_list.select(Some(idx));
        }
    }
    /// Open a highlighted folder: expand its path in the tree and show its
    /// children in the right pane (used by `d`/`→` and double-click).
    fn open_item(&mut self, item: DirOrSong, ctx: &Ctx) -> Result<()> {
        let DirOrSong::Dir { full_path, .. } = item else { return Ok(()) };
        let path = split_path(&full_path);
        self.tree.sync(&path);
        self.selected = Some(path.clone());
        self.fetch_children(&path, ctx);
        self.populate_items();
        self.sync_tree_to_items_cursor();
        ctx.render()?;
        Ok(())
    }
    /// Expand or collapse a tree node. The Library root is never
    /// collapsible. The right pane mirrors the highlighted node: when the
    /// toggled node is the current one, its list is re-shown.
    fn set_expanded(&mut self, path: &Path, expanded: bool, ctx: &Ctx) -> Result<()> {
        if path.is_empty() {
            return Ok(());
        }
        if expanded {
            self.tree.expand(path.as_slice());
        } else {
            self.tree.collapse(path.as_slice());
        }
        if self.selected.as_ref() == Some(path) {
            self.populate_items();
            self.sync_tree_to_items_cursor();
        }
        ctx.render()?;
        Ok(())
    }
    /// Move the tree highlight; the highlighted folder's children fill the
    /// right pane.
    /// Move the right-pane cursor; the left tree follows when the item has
    /// a tree row (folders).
    /// Context menu for a tree folder: add the whole subtree to the queue
    /// or to a playlist.
    fn open_folder_menu(&mut self, path: &Path, ctx: &Ctx) -> Result<()> {
        let path_str = path.to_string();
        let folder_name = path
            .as_slice()
            .last()
            .cloned()
            .unwrap_or_else(|| "Library".to_owned());
        let find_songs = move |client: &mut Client<'_>| -> Result<Vec<Song>> {
            Ok(
                client
                    .find(
                        &[
                            Filter::new_with_kind(
                                Tag::File,
                                &path_str,
                                FilterKind::StartsWith,
                            ),
                        ],
                    )?,
            )
        };
        let modal = MenuModal::new(ctx)
            .list_section(
                ctx,
                |mut section| {
                    let find = find_songs.clone();
                    section
                        .add_item(
                            "Add folder to queue",
                            move |ctx| {
                                ctx.command(move |client| {
                                    let songs = find(client)?;
                                    let items: Vec<Enqueue> = songs
                                        .into_iter()
                                        .map(|s| Enqueue::File { path: s.file })
                                        .collect();
                                    client.enqueue_multiple(items, None, None, false)?;
                                    Ok(())
                                });
                                Ok(())
                            },
                        );
                    let find = find_songs.clone();
                    section
                        .add_item(
                            "Replace queue with folder",
                            move |ctx| {
                                ctx.command(move |client| {
                                    let songs = find(client)?;
                                    let items: Vec<Enqueue> = songs
                                        .into_iter()
                                        .map(|s| Enqueue::File { path: s.file })
                                        .collect();
                                    client.enqueue_multiple(items, None, None, true)?;
                                    Ok(())
                                });
                                Ok(())
                            },
                        );
                    Some(section)
                },
            )
            .list_section(
                ctx,
                |mut section| {
                    let find = find_songs.clone();
                    let initial = folder_name.clone();
                    section
                        .add_item(
                            "Create playlist from folder",
                            move |ctx| {
                                modal!(
                                    ctx, InputModal::new(ctx).title("Create new playlist")
                                    .confirm_label("Save").input_label("Playlist name:")
                                    .initial_value(initial).on_confirm(move | ctx, value | { let
                                    value = value.to_owned(); let find = find.clone(); ctx
                                    .command(move | client | { let songs = find(client) ?;
                                    client.create_playlist(& value, songs.into_iter().map(| s |
                                    s.file).collect(),) ?; Ok(()) }); Ok(()) })
                                );
                                Ok(())
                            },
                        );
                    let find = find_songs.clone();
                    section
                        .add_item(
                            "Add folder to playlist",
                            move |ctx| {
                                let radio_playlist = ctx.config.radio.playlist.clone();
                                let (items, playlists) = ctx
                                    .query_sync(move |client| {
                                        let songs = find(client)?;
                                        let playlists = client
                                            .picker_playlists(&radio_playlist)?
                                            .into_iter()
                                            .map(|p| p.name)
                                            .collect_vec();
                                        Ok((songs, playlists))
                                    })?;
                                modal!(
                                    ctx, SelectModal::builder().ctx(ctx).options(playlists)
                                    .confirm_label("Add").title("Select a playlist")
                                    .on_confirm(move | ctx, selected, _idx | { ctx.command(move
                                    | client | { client.add_to_playlist_multiple(& selected,
                                    items.into_iter().map(| s | s.file).collect_vec(),) ?;
                                    Ok(()) }); Ok(()) }).build()
                                );
                                Ok(())
                            },
                        );
                    Some(section)
                },
            );
        crate::shared::macros::modal!(ctx, modal);
        Ok(())
    }
    /// Context menu for a highlighted file: add it to the queue or to a
    /// playlist. When songs are marked, the menu acts on every marked
    /// song (like the audio queue list's menu); otherwise the highlighted
    /// file.
    fn open_song_menu(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.selected_item() else { return Ok(()) };
        let songs: Vec<Song> = if self.marked.is_empty() {
            match &item {
                DirOrSong::Song(song) => vec![song.clone()],
                DirOrSong::Dir { .. } => Vec::new(),
            }
        } else {
            self.marked
                .iter()
                .filter_map(|idx| match self.items.get(idx) {
                    Some(DirOrSong::Song(song)) => Some(song.clone()),
                    _ => None,
                })
                .collect()
        };
        let current_items: Vec<Enqueue> = songs
            .iter()
            .map(|s| Enqueue::File {
                path: s.file.clone(),
            })
            .collect();
        let list_songs = move |_client: &mut Client<'_>| -> Result<Vec<Song>> {
            Ok(songs.clone())
        };
        let modal = MenuModal::new(ctx)
            .list_section(
                ctx,
                |mut section| {
                    if !current_items.is_empty() {
                        let cloned_items = current_items.clone();
                        section
                            .add_item(
                                "Add to queue",
                                move |ctx| {
                                    ctx.command(move |client| {
                                        client.enqueue_multiple(cloned_items, None, None, false)?;
                                        Ok(())
                                    });
                                    Ok(())
                                },
                            );
                        let cloned_items = current_items.clone();
                        section
                            .add_item(
                                "Replace queue",
                                move |ctx| {
                                    ctx.command(move |client| {
                                        client.enqueue_multiple(cloned_items, None, None, true)?;
                                        Ok(())
                                    });
                                    Ok(())
                                },
                            );
                    }
                    let songs_in_item = list_songs.clone();
                    section
                        .add_item(
                            "Create playlist",
                            move |ctx| {
                                modal!(
                                    ctx, InputModal::new(ctx).title("Create new playlist")
                                    .confirm_label("Save").input_label("Playlist name:")
                                    .on_confirm(move | ctx, value | { let value = value
                                    .to_owned(); ctx.command(move | client | { let songs =
                                    songs_in_item(client) ?; client.create_playlist(& value,
                                    songs.into_iter().map(| s | s.file).collect(),) ?; Ok(())
                                    }); Ok(()) })
                                );
                                Ok(())
                            },
                        );
                    let songs_in_item = list_songs.clone();
                    section
                        .add_item(
                            "Add to playlist",
                            move |ctx| {
                                let radio_playlist = ctx.config.radio.playlist.clone();
                                let (items, playlists) = ctx
                                    .query_sync(move |client| {
                                        let songs = songs_in_item(client)?;
                                        let playlists = client
                                            .picker_playlists(&radio_playlist)?
                                            .into_iter()
                                            .map(|p| p.name)
                                            .collect_vec();
                                        Ok((songs, playlists))
                                    })?;
                                modal!(
                                    ctx, SelectModal::builder().ctx(ctx).options(playlists)
                                    .confirm_label("Add").title("Select a playlist")
                                    .on_confirm(move | ctx, selected, _idx | { ctx.command(move
                                    | client | { client.add_to_playlist_multiple(& selected,
                                    items.into_iter().map(| s | s.file).collect_vec(),) ?;
                                    Ok(()) }); Ok(()) }).build()
                                );
                                Ok(())
                            },
                        );
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
        modal!(ctx, modal);
        Ok(())
    }
    /// Right arrow / `d` (or double-click on a file): play the
    /// highlighted file immediately without adding it to the queue (it is
    /// removed again once the song changes).
    fn play_selected_file(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(DirOrSong::Song(song)) = self.selected_item() else {
            return Ok(());
        };
        self.drop_temp_play(ctx);
        let file = song.file.clone();
        if crate::ui::modals::paste::downloads_dir()
            .is_some_and(|dir| std::path::Path::new(&file).starts_with(&dir))
        {
            let title = std::path::Path::new(&file)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.clone());
            crate::core::mpv::play_video_entries(
                ctx,
                vec![crate ::core::mpv::MpvPlaylistEntry::new(title, file, None)],
            );
            return Ok(());
        }
        self.play_temp_url(
            ctx,
            PLAY_FILE,
            PaneType::Directories {
                tree: TreeBrowserArgs::default(),
            },
            file,
        );
        Ok(())
    }
}
impl TreeBrowserCore for DirectoriesPane {
    type Item = DirOrSong;
    fn tree_rows(&self) -> Vec<TreeRowView> {
        let mut flat = Vec::new();
        self.tree.visible(&self.tree.root, 0, &mut flat);
        flat.into_iter()
            .map(|(node, depth)| TreeRowView {
                label: if node.path.is_empty() {
                    "Library ↴".to_owned()
                } else {
                    node.display_name().to_owned()
                },
                depth: depth as u8,
                expandable: !node.children.is_empty(),
                expanded: node.expanded,
                root: node.path.is_empty(),
            })
            .collect()
    }
    fn tree_selected(&self) -> usize {
        self.tree.selected
    }
    fn tree_list(&self) -> &ListState {
        &self.tree_state
    }
    fn tree_list_mut(&mut self) -> &mut ListState {
        &mut self.tree_state
    }
    fn tree_area(&self) -> Rect {
        self.tree_inner
    }
    fn set_tree_area(&mut self, area: Rect) {
        self.tree_inner = area;
    }
    fn set_expanded_idx(&mut self, idx: usize, expanded: bool, ctx: &Ctx) -> Result<()> {
        let path = {
            let mut flat = Vec::new();
            self.tree.visible(&self.tree.root, 0, &mut flat);
            flat.get(idx).map(|(node, _)| Path::from(node.path.clone()))
        };
        let Some(path) = path else { return Ok(()) };
        self.set_expanded(&path, expanded, ctx)
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
    fn items_area(&self) -> Rect {
        self.items_inner
    }
    fn set_items_area(&mut self, area: Rect) {
        self.items_inner = area;
    }
    fn item_at(&self, idx: usize) -> Option<Self::Item> {
        self.items.get(idx).cloned()
    }
    fn item_row(&self, idx: usize, hovered: bool, ctx: &Ctx) -> ListItem<'static> {
        let is_marked = self.marked.contains(idx);
        let item = match &self.items[idx] {
            DirOrSong::Dir { name, .. } => {
                ListItem::from(
                    Line::from(
                        vec![
                            Span::from("▶ "), Span::from(if name.is_empty() {
                            "Untitled".to_owned() } else { name.clone() }),
                        ],
                    ),
                )
            }
            DirOrSong::Song(song) => {
                let spans: Vec<Span> = ctx
                    .config
                    .theme
                    .browser_song_format
                    .0
                    .iter()
                    .map(|prop| {
                        Span::from(
                            prop
                                .as_string(
                                    Some(song),
                                    &ctx.config.theme.format_tag_separator,
                                    ctx.config.theme.multiple_tag_resolution_strategy,
                                    ctx,
                                )
                                .unwrap_or_default(),
                        )
                    })
                    .collect();
                ListItem::from(Line::from(spans))
            }
        };
        if is_marked {
            item.style(ctx.config.theme.marked_item_style)
        } else if hovered {
            item.style(ctx.config.theme.hovered_item_style)
        } else {
            item
        }
    }
    fn highlight_tree_node(&mut self, idx: usize, ctx: &Ctx) -> Result<()> {
        let node_path: Option<Vec<String>> = {
            let mut flat = Vec::new();
            self.tree.visible(&self.tree.root, 0, &mut flat);
            flat.get(idx).map(|(node, _)| node.path.clone())
        };
        let Some(node_path) = node_path else { return Ok(()) };
        self.tree.selected = idx;
        if node_path.is_empty() {
            self.selected = None;
            self.fetch_children(&Path::new(), ctx);
        } else {
            let path = Path::from(node_path);
            self.selected = Some(path.clone());
            self.fetch_children(&path, ctx);
        }
        self.populate_items();
        ctx.render()?;
        Ok(())
    }
    fn select_parent(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(current) = self.selected.clone() else { return Ok(()) };
        if current.is_empty() {
            return Ok(());
        }
        let prev = current.clone();
        self.tree.collapse(prev.as_slice());
        let mut parent = current;
        parent.pop();
        self.selected = if parent.is_empty() { None } else { Some(parent.clone()) };
        self.fetch_children(&parent, ctx);
        self.populate_items();
        self.select_items_item(&prev);
        self.scroll_items_selection_into_view(ctx);
        self.sync_tree_to_items_cursor();
        self.scroll_tree_selection_into_view(ctx);
        ctx.render()?;
        Ok(())
    }
    fn activate_selected(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.selected_item() else { return Ok(()) };
        if item.is_file() {
            self.play_selected_file(ctx)
        } else {
            self.open_item(item, ctx)
        }
    }
    fn open_context_menu(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.selected_item() else { return Ok(()) };
        if item.is_file() {
            self.open_song_menu(ctx)
        } else {
            let DirOrSong::Dir { full_path, .. } = item else { return Ok(()) };
            let path = split_path(&full_path);
            self.open_folder_menu(&path, ctx)
        }
    }
    fn render_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let mut items: Vec<ListItem> = Vec::new();
        if let Some(selected) = self.selected_item() {
            match selected {
                DirOrSong::Song(song) => {
                    for group in song.to_file_preview(ctx) {
                        if let Some(name) = group.name {
                            items
                                .push(
                                    ListItem::new(
                                        Line::styled(name, group.header_style.unwrap_or_default()),
                                    ),
                                );
                        }
                        items.extend(group.items);
                        items.push(ListItem::new(""));
                    }
                }
                DirOrSong::Dir { name, full_path, last_modified, .. } => {
                    let key = ctx.config.theme.preview_label_style;
                    let group = ctx.config.theme.preview_metadata_group_style;
                    items.push(ListItem::new(Line::styled(" --- [Folder]", group)));
                    items
                        .push(
                            ListItem::new(
                                Line::from(
                                    vec![
                                        Span::styled("Name", key), Span::raw(": "), Span::raw(name
                                        .clone()),
                                    ],
                                ),
                            ),
                        );
                    items
                        .push(
                            ListItem::new(
                                Line::from(
                                    vec![
                                        Span::styled("Path", key), Span::raw(": "),
                                        Span::raw(full_path.clone()),
                                    ],
                                ),
                            ),
                        );
                    items
                        .push(
                            ListItem::new(
                                Line::from(
                                    vec![
                                        Span::styled("Last Modified", key), Span::raw(": "),
                                        Span::raw(last_modified.to_string()),
                                    ],
                                ),
                            ),
                        );
                }
            }
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        ratatui::widgets::Widget::render(
            List::new(items).block(block).style(ctx.config.as_list_name_style()),
            area,
            frame.buffer_mut(),
        );
    }
    fn temp_play_id(&self) -> Option<u32> {
        self.temp_play_id
    }
    fn set_temp_play_id(&mut self, id: Option<u32>) {
        self.temp_play_id = id;
    }
    fn tree_title(&self) -> &'static str {
        " Folders "
    }
    /// The MPD tree arrows: ▾/▸ for folders with subdirectories, a
    /// two-space spacer for leaves so the names stay aligned (no ▼/▶).
    fn tree_arrow(&self, row: &TreeRowView) -> &'static str {
        if row.expandable { if row.expanded { "▾ " } else { "▸ " } } else { "  " }
    }
    fn items_title(&self) -> String {
        match self.selected.as_ref() {
            Some(path) => {
                path.current_dir()
                    .map_or_else(
                        || " Library".to_owned(),
                        |name| {
                            if name == crate::ui::modals::paste::DOWNLOADS_DIR_NAME {
                                " Downloads".to_owned()
                            } else {
                                format!(" {name}")
                            }
                        },
                    )
            }
            None => " Library".to_owned(),
        }
    }
    fn tips_lines(&self, ctx: &Ctx) -> Vec<Line<'static>> {
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        vec![
            Line::from(vec![Span::styled("w/s · ↑/↓", base),
            Span::styled("  folders · items", dim),]),
            Line::from(vec![Span::styled("d / a", base),
            Span::styled("  open · back out", dim)]),
            Line::from(vec![Span::styled("Enter", base), Span::styled("  context menu",
            dim)]), Line::from(vec![Span::styled("d / →", base),
            Span::styled("  open · play", dim)]),
        ]
    }
    /// The info box takes about two thirds of the pane height (the tips
    /// strip stays a fixed 3 rows); the item list gets the rest. Exact
    /// lengths are computed so the rows always fill the area exactly.
    fn layout_vertical(&self, right: Rect) -> (Rect, Rect, Rect) {
        let tips_h = 3;
        let info_h = self
            .tree_args
            .info_box_height(right.height.saturating_sub(tips_h) * 2 / 3);
        let files_h = right.height.saturating_sub(tips_h + info_h);
        let [files_area, tips_area, info_area] = Layout::vertical([
                Constraint::Length(files_h),
                Constraint::Length(tips_h),
                Constraint::Length(info_h),
            ])
            .areas(right);
        (files_area, tips_area, info_area)
    }
    /// The configured tree-browser args drive the shared `split_tree`
    /// (tree min width / hide threshold).
    fn tree_args(&self) -> TreeBrowserArgs {
        self.tree_args.clone()
    }
    /// Keep the tree highlight on the right-pane cursor: the highlighted
    /// item's row in the tree when it has one (a folder), otherwise the
    /// current node's row (songs have no tree row).
    fn sync_tree_to_items_cursor(&mut self) {
        let target = self
            .selected_item()
            .and_then(|item| match item {
                DirOrSong::Dir { full_path, .. } => Some(split_path(&full_path)),
                DirOrSong::Song(_) => None,
            })
            .or_else(|| self.selected.clone());
        let Some(target) = target else { return };
        let idx = {
            let mut flat = Vec::new();
            self.tree.visible(&self.tree.root, 0, &mut flat);
            let by_item = flat.iter().position(|(n, _)| n.path == target.as_slice());
            let by_node = self
                .selected
                .as_ref()
                .and_then(|selected| {
                    flat.iter().position(|(n, _)| n.path == selected.as_slice())
                });
            by_item.or(by_node)
        };
        if let Some(idx) = idx {
            self.tree.selected = idx;
        }
    }
    fn on_confirm(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.open_context_menu(ctx)
    }
    fn on_select_range(&mut self, dir: i64, ctx: &mut Ctx) -> Result<bool> {
        let start = self.item_list.selected().unwrap_or(0);
        if self.marked.anchor().is_none() || self.marked.is_empty() {
            self.marked.set_anchor(start);
        }
        self.move_items(dir, ctx)?;
        let sel = self.item_list.selected().unwrap_or(start);
        self.marked.select_range(sel);
        ctx.render()?;
        Ok(true)
    }
    fn on_close(&mut self, ctx: &Ctx) -> Result<bool> {
        if !self.marked.is_empty() {
            self.marked.clear();
            self.marked.clear_anchor();
            ctx.render()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn on_select_all(&mut self, ctx: &mut Ctx) -> Result<bool> {
        let len = self.items_len();
        if len > 0 {
            self.marked.mark_all(len);
            ctx.render()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn handle_items_left_click(
        &mut self,
        row: usize,
        event: &MouseEvent,
        ctx: &Ctx,
    ) -> Result<()> {
        if row >= self.items_len() {
            return Ok(());
        }
        if event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
            if self.marked.is_empty() {
                if let Some(sel) = self.item_list.selected() {
                    self.marked.add(sel);
                }
            }
            self.marked.add(row);
            self.item_list.select(Some(row));
            self.sync_tree_to_items_cursor();
            ctx.render()?;
        } else if event.modifiers.contains(crossterm::event::KeyModifiers::ALT) {
            if self.marked.anchor().is_none() {
                self.marked.set_anchor(row);
            }
            self.marked.select_range(row);
            self.item_list.select(Some(row));
            self.sync_tree_to_items_cursor();
            ctx.render()?;
        } else {
            if !self.marked.is_empty() && Some(row) != self.item_list.selected() {
                self.marked.clear();
            }
            self.item_list.select(Some(row));
            self.marked.set_anchor(row);
            self.marked.clear_range();
            self.sync_tree_to_items_cursor();
            ctx.render()?;
        }
        Ok(())
    }
    fn tree_context_menu(&mut self, idx: usize, ctx: &Ctx) -> Result<()> {
        let path = {
            let mut flat = Vec::new();
            self.tree.visible(&self.tree.root, 0, &mut flat);
            flat.get(idx).map(|(node, _)| Path::from(node.path.clone()))
        };
        if let Some(path) = path { self.open_folder_menu(&path, ctx) } else { Ok(()) }
    }
    fn on_reconnected(&mut self, ctx: &Ctx) -> Result<()> {
        self.initialized = false;
        self.before_show(ctx)?;
        self.temp_play_id = None;
        Ok(())
    }
}
impl Pane for DirectoriesPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        let [toggle_area, content] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(area);
        self.render_toggle(frame, toggle_area, ctx);
        match self.mode {
            MpdTabMode::Library => self.render_tree_browser(frame, content, ctx),
            MpdTabMode::Search => self.search.render(frame, content, ctx),
        }
    }
    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.initialized {
            self.fetch_children(&Path::new(), ctx);
            ctx.query()
                .id(TREE)
                .replace_id(TREE)
                .target(PaneType::Directories {
                    tree: TreeBrowserArgs::default(),
                })
                .query(move |client| {
                    let dirs: Vec<String> = client
                        .list_all(None)?
                        .into_iter()
                        .filter_map(|e| match e {
                            crate::mpd::commands::list_all::ListAllEntry::Dir(p) => {
                                Some(p)
                            }
                            _ => None,
                        })
                        .collect();
                    Ok(MpdQueryResult::Any(Box::new(dirs)))
                });
            self.initialized = true;
        }
        Ok(())
    }
    fn on_event(
        &mut self,
        event: &mut UiEvent,
        is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        let _tree_handled = self.handle_tree_events(event, is_visible, ctx)?;
        match event {
            UiEvent::Database => {
                self.loaded.clear();
                self.pending.clear();
                self.selected = None;
                self.items.clear();
                self.item_list.select(None);
                self.marked.clear();
                self.initialized = false;
                self.before_show(ctx)?;
            }
            _ => {}
        }
        self.search.on_event(event, is_visible, ctx)?;
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if matches!(
            event.kind, MouseEventKind::LeftClick | MouseEventKind::DoubleClick
        ) {
            for (idx, area) in self.toggle_areas.iter().enumerate() {
                if area.contains(event.into()) {
                    let mode = if idx == 0 {
                        MpdTabMode::Library
                    } else {
                        MpdTabMode::Search
                    };
                    return self.set_mode(mode, ctx);
                }
            }
        }
        match self.mode {
            MpdTabMode::Library => self.handle_tree_mouse_event(event, ctx),
            MpdTabMode::Search => self.search.handle_mouse_event(event, ctx),
        }
    }
    fn handle_insert_mode(
        &mut self,
        kind: InputResultEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
        match self.mode {
            MpdTabMode::Library => Ok(()),
            MpdTabMode::Search => self.search.handle_insert_mode(kind, ctx),
        }
    }
    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_global() {
            if matches!(action, GlobalAction::ToggleMpdMode) {
                return self.toggle_mode(ctx);
            }
            event.abandon();
        }
        match self.mode {
            MpdTabMode::Library => {
                self.handle_tree_action(event, ctx)?;
            }
            MpdTabMode::Search => self.search.handle_action(event, ctx)?,
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
        match id {
            FETCH_DATA => {
                let MpdQueryResult::DirOrSong { data, path } = data else {
                    log::error!(id; "Unexpected result for the children fetch");
                    return Ok(());
                };
                let Some(path) = path else {
                    log::error!(
                        path:?; "Cannot insert data because path is not provided"
                    );
                    return Ok(());
                };
                self.pending.clear();
                self.loaded.insert(path.clone(), data);
                let is_current = self.selected.as_ref() == Some(&path)
                    || (self.selected.is_none() && path.is_empty());
                if is_current {
                    self.populate_items();
                    self.sync_tree_to_items_cursor();
                    self.scroll_tree_selection_into_view(ctx);
                    ctx.render()?;
                }
            }
            TREE => {
                let MpdQueryResult::Any(any) = data else { return Ok(()) };
                if let Ok(dirs) = any.downcast::<Vec<String>>() {
                    self.tree = DirTree::build(dirs.into_iter());
                    self.tree
                        .root
                        .children
                        .insert(
                            0,
                            TreeNode {
                                name: crate::ui::modals::paste::DOWNLOADS_DIR_NAME
                                    .to_owned(),
                                path: vec![
                                    crate ::ui::modals::paste::DOWNLOADS_DIR_NAME.to_owned()
                                ],
                                display: Some("Downloads".to_owned()),
                                children: Vec::new(),
                                expanded: false,
                            },
                        );
                    ctx.render()?;
                }
            }
            PLAY_FILE => {
                let MpdQueryResult::Any(any) = data else { return Ok(()) };
                if let Ok(id) = any.downcast::<u32>() {
                    self.temp_play_id = Some(*id);
                    ctx.temp_play_id.set(Some(*id));
                }
            }
            _ => self.search.on_query_finished(id, data, _is_visible, ctx)?,
        }
        Ok(())
    }
}
impl DirectoriesPane {}

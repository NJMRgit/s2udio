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
            info_list_modal::{INFO_COLUMN_WIDTHS, InfoListModal, song_info_rows},
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
#[allow(dead_code)] // the parity test that used it is deferred (see HANDOFF round 99b)
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
    /// Rubber-band (drag-rect) selection state of the items list (Round
    /// 46): armed by a left press, updated by `Drag` events, finalized by
    /// `LeftRelease`.
    items_band: crate::ui::band::BandState,
    /// Drag state of the items-list scrollbar (Round 48: press-and-hold on
    /// the thumb/track, then the thumb follows the pointer anywhere).
    item_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag,
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
    /// Click area of the `↰ Back` button (round 60 B3), refreshed on every
    /// render; zero when hidden.
    back_area: Rect,
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
            items_band: crate::ui::band::BandState::default(),
            item_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            selected: None,
            tree_args: ctx.config.tree_browser_args(PaneTypeDiscriminants::Directories),
            loaded: HashMap::new(),
            pending: HashSet::new(),
            temp_play_id: None,
            initialized: false,
            mode: MpdTabMode::Library,
            search: SearchPane::new(ctx),
            toggle_areas: [Rect::default(); 2],
            back_area: Rect::default(),
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
        // Round 60 (B3): `↰ Back` at the right end of the toggle row,
        // visible only while a subdirectory is shown.
        let visible = self.mode == MpdTabMode::Library && self.selected.is_some();
        self.back_area = crate::ui::draw_back_button(frame, area, visible, ctx);
    }
    /// Fetch the children of `path` ("" = root): the folders + songs shown
    /// in the right pane. Playlists are handled by the Playlists tab and
    /// hidden directories are never listed; the MPD browser shows folders
    /// and songs only. The Downloads folder is the exception: it lives
    /// outside the MPD library, so its listing comes from disk instead of
    /// MPD `lsinfo` (and is never cached — downloads appear/disappear).
    fn fetch_children(&mut self, path: &Path, ctx: &Ctx) {
        // Round 64: the Downloads folder is NEVER cached — it is re-listed
        // from disk on every fetch (its contents live outside the MPD
        // library, so MPD cannot notify us; files appear/disappear while
        // the browser sits on it).
        if is_downloads_path(path) {
            self.loaded.insert(path.clone(), list_downloads_dir());
            return;
        }
        if self.pending.contains(path) || self.loaded.contains_key(path) {
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
        let items: Vec<DirOrSong> = match self.selected.as_ref() {
            Some(path) => self.loaded.get(path).cloned().unwrap_or_default(),
            None => self.loaded.get(&Path::new()).cloned().unwrap_or_default(),
        }
            .into_iter()
            .filter(is_visible_entry)
            .collect();
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
    /// Context menu for one or more folders (round 60 C2: a multi-selection
    /// of folders is served the same whole-subtree actions as a single
    /// folder, applied to every selected folder). Actions: Add to queue /
    /// Replace queue with items, Create playlist, Add to playlist — all
    /// recursive per folder, like the single-folder menu.
    fn open_folders_menu(
        &mut self,
        paths: Vec<Path>,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        let plural = paths.len() > 1;
        let folder_name = paths
            .first()
            .and_then(|p| p.as_slice().last().cloned())
            .unwrap_or_else(|| "Library".to_owned());
        // One `find` (Tag::File StartsWith) per folder path — the MPD
        // filter list is ANDed, so each folder must be queried separately
        // and the results concatenated.
        let find_songs = move |client: &mut Client<'_>| -> Result<Vec<Song>> {
            let path_strs: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
            let mut out = Vec::new();
            for path_str in path_strs {
                out.extend(
                    client.find(&[Filter::new_with_kind(Tag::File, &path_str, FilterKind::StartsWith)])?,
                );
            }
            Ok(out)
        };
        let add_queue_label =
            if plural { "Add folders to queue" } else { "Add folder to queue" };
        let replace_label = if plural {
            "Replace queue with folders"
        } else {
            "Replace queue with folder"
        };
        let create_label = if plural {
            "Create playlist from folders"
        } else {
            "Create playlist from folder"
        };
        let add_playlist_label = if plural {
            "Add folders to playlist"
        } else {
            "Add folder to playlist"
        };
        let modal = MenuModal::new(ctx)
            .anchor(anchor)
            .list_section(
                ctx,
                |mut section| {
                    let find = find_songs.clone();
                    section
                        .add_item(
                            add_queue_label,
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
                            replace_label,
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
                            create_label,
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
                            add_playlist_label,
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
    /// Context menu for a highlighted folder (single path).
    fn open_folder_menu(
        &mut self,
        path: &Path,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        self.open_folders_menu(vec![path.clone()], ctx, anchor)
    }
    /// Context menu for a highlighted file: add it to the queue or to a
    /// playlist. When songs are marked, the menu acts on every marked
    /// song (like the audio queue list's menu); otherwise the highlighted
    /// file.
    fn open_song_menu(
        &mut self,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
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
            .anchor(anchor)
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
    /// Rubber-band motion of the items list (Round 46): plain drag
    /// replaces every mark with the anchor→current range, ctrl+drag
    /// adds/contracts the range keeping the other marks; release
    /// finalizes (or applies the deferred plain-click unmark when the
    /// press never moved).
    fn handle_items_band(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        match event.kind {
            MouseEventKind::Drag { .. } => {
                if !self.items_band.is_active() {
                    return Ok(());
                }
                let control =
                    event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL);
                let area = self.items_area();
                let (offset, len) = (self.item_list.offset(), self.items_len());
                let Some(current) =
                    crate::ui::band::band_current_row(event.y, area, offset, len, 1)
                else {
                    return Ok(());
                };
                let anchor = self.items_band.anchor.unwrap_or(current);
                self.items_band.update(current);
                // The band anchor drives the range even after keyboard
                // selection set the mark anchor elsewhere.
                self.marked.set_anchor(anchor);
                if control {
                    self.marked.select_range(current);
                } else {
                    self.marked.clear();
                    self.marked.set_anchor(anchor);
                    self.marked.select_range(current);
                }
                ctx.render()?;
            }
            MouseEventKind::LeftRelease => {
                let clear_marks = match self.items_band.release() {
                    crate::ui::band::BandEnd::Click { clear_marks } => clear_marks,
                    _ => false,
                };
                if clear_marks {
                    self.marked.clear();
                }
                ctx.render()?;
            }
            _ => {}
        }
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
    fn on_tree_hidden(&mut self) {
        // Round 60 (C1): the stored tree rect must not linger once the tree
        // pane is hidden (TUI <= 120 cols). A stale rect would swallow
        // clicks over the items pane (they would hit the invisible tree's
        // hit-test and act as "left-pane" control).
        self.tree_inner = Rect::default();
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
    fn items_band(&self) -> Option<&crate::ui::band::BandState> {
        Some(&self.items_band)
    }
    fn items_band_mut(&mut self) -> Option<&mut crate::ui::band::BandState> {
        Some(&mut self.items_band)
    }
    fn items_scrollbar_drag(&mut self) -> &mut crate::shared::mouse_event::ScrollbarDrag {
        &mut self.item_scrollbar_drag
    }
    /// Round 89: "show info" on the highlighted file / folder row — the
    /// detailed song panel for a file, the folder's own facts for a
    /// directory. The Directories pane's rows are the raw library tree, so
    /// the info box's preview is not the whole story.
    fn show_info(&self, ctx: &Ctx) -> Result<bool> {
        let Some(idx) = self.item_list.selected() else { return Ok(false) };
        let Some(item) = self.items.get(idx) else { return Ok(false) };
        let rows = match item {
            DirOrSong::Song(song) => song_info_rows(ctx, song),
            DirOrSong::Dir { name, full_path, .. } => {
                let path = if full_path.is_empty() { name } else { full_path };
                crate::ui::song_list::directory_info_rows(path)
            }
        };
        modal!(
            ctx, InfoListModal::builder().rows(rows).title("Info")
            .column_widths(INFO_COLUMN_WIDTHS).build()
        );
        ctx.render()?;
        Ok(true)
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
    fn open_context_menu(
        &mut self,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        // Round 60 (C2): a multi-selection of folders gets the whole-
        // selection folder menu (Add to queue / Replace queue with the
        // recursive contents of every selected folder), alongside the
        // existing folder actions. Enter opens the same menu (on_confirm).
        let marked_folders: Vec<Path> = self
            .marked
            .iter()
            .filter_map(|idx| match self.items.get(idx) {
                Some(DirOrSong::Dir { full_path, .. }) => Some(split_path(full_path)),
                _ => None,
            })
            .collect();
        if !marked_folders.is_empty()
            && marked_folders.len() == self.marked.iter().count()
        {
            return self.open_folders_menu(marked_folders, ctx, anchor);
        }
        let Some(item) = self.selected_item() else { return Ok(()) };
        if item.is_file() {
            self.open_song_menu(ctx, anchor)
        } else {
            let DirOrSong::Dir { full_path, .. } = item else { return Ok(()) };
            let path = split_path(&full_path);
            self.open_folder_menu(&path, ctx, anchor)
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
            .border_set(ctx.config.as_border_set())
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
    /// Round 60c (S3): the legend/tips strip is gone — the info box
    /// takes about two thirds of the pane height (fractional heights
    /// compute so the rows always fill the area exactly); the item list
    /// gets the rest.
    fn layout_vertical(&mut self, right: Rect) -> (Rect, Rect) {
        let info_h = self
            .tree_args
            .info_box_height(right.height * 2 / 3);
        let files_h = right.height.saturating_sub(info_h);
        let [files_area, info_area] = Layout::vertical([
                Constraint::Length(files_h),
                Constraint::Length(info_h),
            ])
            .areas(right);
        (files_area, info_area)
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
        self.open_context_menu(ctx, None)
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
            // Round 47: a press in the empty pane space below the items
            // arms the band at the clamped edge row, so a drag can select
            // from empty space into the list, and a plain click there
            // clears the multi-selection (the release's deferred plain-
            // click path applies it). The selection cursor stays put.
            // Ctrl/alt presses have no row to act on — only the plain arm.
            if !event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                && !event.modifiers.contains(crossterm::event::KeyModifiers::ALT)
            {
                let area = self.items_area();
                let Some(edge) = crate::ui::band::band_current_row(
                    event.y,
                    area,
                    self.item_list.offset(),
                    self.items_len(),
                    1,
                ) else {
                    return Ok(());
                };
                let clear_marks = !self.marked.is_empty();
                self.items_band.arm(edge, clear_marks);
                ctx.render()?;
            }
            return Ok(());
        }
        if event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
            // Ctrl+click toggles the row: mark it if it was not marked,
            // unmark it if it was.
            let was_marked = self.marked.contains(row);
            if self.marked.is_empty() {
                if let Some(sel) = self.item_list.selected() {
                    self.marked.add(sel);
                }
            }
            if was_marked {
                self.marked.remove(row);
            } else {
                self.marked.add(row);
            }
            // Arm the band so a ctrl+drag from here adds a range.
            self.items_band.arm(row, false);
            self.item_list.select(Some(row));
            self.sync_tree_to_items_cursor();
            ctx.render()?;
        } else if event.modifiers.contains(crossterm::event::KeyModifiers::ALT) {
            self.items_band.cancel();
            if self.marked.anchor().is_none() {
                self.marked.set_anchor(row);
            }
            self.marked.select_range(row);
            self.item_list.select(Some(row));
            self.sync_tree_to_items_cursor();
            ctx.render()?;
        } else {
            // A plain press arms the band and defers the multi-selection
            // drop (click ≠ drag); the release resolves it (Round 46).
            let click_on_different_row =
                !self.marked.is_empty() && Some(row) != self.item_list.selected();
            self.items_band.arm(row, click_on_different_row);
            self.item_list.select(Some(row));
            self.marked.set_anchor(row);
            self.marked.clear_range();
            self.sync_tree_to_items_cursor();
            ctx.render()?;
        }
        Ok(())
    }
    fn tree_context_menu(
        &mut self,
        idx: usize,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        let path = {
            let mut flat = Vec::new();
            self.tree.visible(&self.tree.root, 0, &mut flat);
            flat.get(idx).map(|(node, _)| Path::from(node.path.clone()))
        };
        if let Some(path) = path {
            self.open_folder_menu(&path, ctx, anchor)
        } else {
            Ok(())
        }
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
        if self.back_area.width > 0
            && matches!(
                event.kind, MouseEventKind::LeftClick | MouseEventKind::DoubleClick
            ) && self.back_area.contains(event.into())
        {
            self.items_band.cancel();
            return self.select_parent(ctx);
        }
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
                    self.items_band.cancel();
                    return self.set_mode(mode, ctx);
                }
            }
        }
        // Band capture (Round 46): once a band is armed in the items
        // list, drags and releases are accepted even when the pointer
        // left the list area (the row clamps to the visible list).
        if self.mode == MpdTabMode::Library && self.items_band.is_active() {
            match event.kind {
                MouseEventKind::Drag { .. } | MouseEventKind::LeftRelease => {
                    return self.handle_items_band(event, ctx);
                }
                _ => {}
            }
        }
        match self.mode {
            MpdTabMode::Library => self.handle_tree_mouse_event(event, ctx),
            MpdTabMode::Search => self.search.handle_mouse_event(event, ctx),
        }
    }
    /// Round 63.1 (3): any release ends an armed scrollbar grab (the
    /// routed release may have landed on another pane).
    fn on_global_mouse_release(&mut self, _ctx: &Ctx) -> Result<()> {
        self.item_scrollbar_drag.disarm();
        Ok(())
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
            // Round 73.2 (revision B: `S`): jump to this tab's search page
            // with the caret in the "Any tag" filter row. `set_mode` (not
            // `toggle_mode`), so the key always lands on Search and never
            // flips back; the session's filters and results are kept — only
            // the field selection and the caret move.
            if matches!(action, GlobalAction::LibrarySearch) {
                self.set_mode(MpdTabMode::Search, ctx)?;
                self.search.focus_any_tag_input(ctx);
                ctx.render()?;
                return Ok(());
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
                    // Round 60 (amendment): the Downloads folder moved out
                    // into its own tab (stream/torrent downloads); the MPD
                    // tree lists the library only.
                    self.tree = DirTree::build(dirs.into_iter());
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




#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::keys::{CommonAction, DirectoriesActions, GlobalAction},
        shared::keys::Actions,
    };

    fn dir(name: &str, full_path: &str) -> DirOrSong {
        DirOrSong::Dir {
            name: name.to_owned(),
            full_path: full_path.to_owned(),
            last_modified: chrono::Utc::now(),
            playlist: false,
        }
    }

    fn song(file: &str) -> DirOrSong {
        DirOrSong::Song(crate::mpd::commands::Song { file: file.to_owned(), ..Default::default() })
    }

    /// The Downloads folder listing reads the real folder from disk:
    /// regular files only (no subfolders/dotfiles), absolute paths, the
    /// file stem as the title, sorted by name.
    #[test]
    fn downloads_listing_reads_the_folder_from_disk() {
        let dir = std::env::temp_dir().join(format!("s2u-dl-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.mp4"), b"b").unwrap();
        std::fs::write(dir.join("a.mp4"), b"a").unwrap();
        std::fs::write(dir.join(".hidden"), b"h").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();

        let items = list_dir(&dir);
        let shown: Vec<&str> = items.iter().map(|i| i.as_path()).collect();
        assert_eq!(
            shown,
            [&dir.join("a.mp4").to_string_lossy(), &dir.join("b.mp4").to_string_lossy()]
        );
        let DirOrSong::Song(song) = &items[0] else { panic!("files list as songs") };
        assert_eq!(song.metadata.get("title").map(|t| t.first()), Some("a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_ctx() -> Ctx {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        // Keep the receivers alive for the whole test: pane actions send
        // render requests and MPD queries that must never fail.
        std::mem::forget(app_rx);
        std::mem::forget(work_rx);
        std::mem::forget(client_rx);
        crate::tests::fixtures::ctx(
            (app_tx, crossbeam::channel::unbounded().1),
            (work_tx, crossbeam::channel::unbounded().1),
            (client_tx, crossbeam::channel::unbounded().1),
        )
    }

    fn action(actions: Vec<Actions>) -> ActionEvent {
        ActionEvent::from(std::sync::Arc::new(actions))
    }






    /// The tree renders the root as `Library ↴` (no collapsible arrow) and
    /// highlights the row the cursor sits on.
    #[test]
    fn tree_renders_the_root_as_library() {
        let ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.tree = DirTree::build(["A/B".to_owned(), "B".to_owned()].into_iter());
        pane.tree.selected = 1; // "A"

        let backend = ratatui::backend::TestBackend::new(160, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                pane.render(frame, Rect::new(0, 0, 160, 10), &ctx).unwrap();
            })
            .unwrap();
        let text = terminal.backend().buffer();
        let mut lines = Vec::new();
        for y in 0..10u16 {
            let line: String = (0..160).map(|x| text[(x, y)].symbol().to_string()).collect();
            lines.push(line);
        }
        let joined = lines.join("\n");
        assert!(joined.contains("Library ↴"), "{joined}");
        assert!(!joined.contains("▾ Library"), "no collapsible arrow on the root: {joined}");
        assert!(joined.contains("▸ A"), "a collapsed folder keeps its arrow: {joined}");
    }



    
    use crate::{
        shared::{
            keys::ActionEvent,
            mouse_event::{MouseEvent, MouseEventKind},
        },
        ui::panes::Pane,
    };
    use crossterm::event::KeyModifiers;
    use ratatui::prelude::Rect;

    fn pane_with_items(ctx: &Ctx) -> DirectoriesPane {
        let mut pane = DirectoriesPane::new(ctx);
        pane.items = vec![
            dir("Folder", "Folder"),
            song("a.flac"),
            song("b.flac"),
            song("c.flac"),
            song("d.flac"),
        ];
        pane.item_list.select(Some(0));
        pane
    }

    #[test]
    fn shift_up_down_ranges_and_contracts() {
        let mut ctx = test_ctx();
        let mut pane = pane_with_items(&ctx);

        // Shift+Down from row 0 marks 0..=1, then extends to 2.
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![0, 1]);
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![0, 1, 2]);

        // Shift+Up contracts, unmarking the row left behind.
        let mut ev = action(vec![Actions::Common(CommonAction::SelectUp)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(pane.item_list.selected(), Some(1));
    }

    #[test]
    fn shift_selection_reanchors_after_the_marks_are_cleared() {
        let mut ctx = test_ctx();
        let mut pane = pane_with_items(&ctx);

        // Shift+Down from row 0 marks 0..=1 (anchor 0).
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![0, 1]);

        // The selection is cleared (Esc) and the cursor moves away: the
        // stale anchor must not survive, so the next Shift+Down starts a
        // fresh range from the new cursor position.
        pane.marked.clear();
        pane.item_list.select(Some(3));
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.item_list.selected(), Some(4));
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![3, 4]);
    }


    /// Round 32: the wheel over the MPD items pane scrolls the viewport
    /// only — the highlight stays put (it may leave the visible area) and
    /// the offset clamps at the list's ends.
    #[test]
    fn wheel_scrolls_the_items_viewport() {
        let ctx = test_ctx();
        let mut pane = pane_with_items(&ctx);
        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 30), &ctx).unwrap()).unwrap();
        // A short viewport so the 5-entry list overflows it.
        let area = pane.items_area();
        pane.set_items_area(Rect::new(area.x, area.y, area.width, 2));

        let selected = pane.item_list.selected();
        assert_eq!(selected, Some(0), "the list opens on the first row");
        let wheel = |pane: &mut DirectoriesPane, kind: MouseEventKind| {
            pane.handle_mouse_event(
                MouseEvent { x: area.x + 1, y: area.y + 1, kind, modifiers: KeyModifiers::NONE },
                &ctx,
            )
            .unwrap();
        };

        wheel(&mut pane, MouseEventKind::ScrollDown);
        assert_eq!(pane.item_list.offset(), 1, "wheel down scrolls the viewport");
        assert_eq!(pane.item_list.selected(), selected, "the highlight does not move");
        wheel(&mut pane, MouseEventKind::ScrollUp);
        assert_eq!(pane.item_list.offset(), 0, "wheel up scrolls back");
        wheel(&mut pane, MouseEventKind::ScrollUp);
        assert_eq!(pane.item_list.offset(), 0, "wheel up clamps at the top");
        for _ in 0..20 {
            wheel(&mut pane, MouseEventKind::ScrollDown);
        }
        assert_eq!(
            pane.item_list.offset(),
            pane.items.len().saturating_sub(2),
            "wheel down clamps at the last page"
        );
        assert_eq!(pane.item_list.selected(), selected, "the highlight never moved");
    }

    #[test]
    fn ctrl_a_marks_every_item_of_the_right_pane() {
        let mut ctx = test_ctx();
        let mut pane = pane_with_items(&ctx);
        assert!(pane.marked.is_empty());

        let mut ev = action(vec![Actions::Common(CommonAction::SelectAll)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);

        // Ctrl+A again keeps everything marked.
        let mut ev = action(vec![Actions::Common(CommonAction::SelectAll)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.marked.iter().count(), 5);
    }


    /// The items-box title keeps the pre-Phase-2 directories spacing: a
    /// single leading space before the name, then the `(n)` count with no
    /// extra space — `" Library(3) "` (Phase 2.1 parity close-out).
    #[test]
    fn items_box_title_keeps_the_directories_spacing() {
        let ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.items = vec![song("a.flac"), song("b.flac"), song("c.flac")];

        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 30), &ctx).unwrap()).unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..30u16)
            .map(|y| (0..80u16).map(|x| buffer[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        assert!(
            text.contains(" Library(3) "),
            "the items-box title is ` Library(3) `, not ` Library (3) ` or `Library(3) `: {text}"
        );
    }

    /// The MPD browser's temp-play entry is dropped on the stop transition
    /// (the shared tree-browser core; pins the Phase-2 fix that gave
    /// directories the Stop cleanup radio/jellyfin already had — before it
    /// the entry leaked into the queue after Stop).
    #[test]
    fn stop_drops_the_temp_play_entry() {
        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.temp_play_id = Some(7);
        ctx.temp_play_id.set(Some(7));
        ctx.status.state = crate::mpd::commands::State::Stop;

        let mut event = crate::ui::UiEvent::PlaybackStateChanged;
        pane.on_event(&mut event, true, &ctx).unwrap();
        assert_eq!(pane.temp_play_id, None);
        assert_eq!(ctx.temp_play_id.get(), None, "the queue pane reads it via Ctx");
    }



    /// On TUIs wider than 120 columns the left folder tree keeps a
    /// 50-column minimum (the 30% share is applied but never below 50),
    /// and the right pane still gets the remainder.
    #[test]
    fn tree_pane_keeps_min_width_on_wide_tui() {
        // 30% of 200 is 60 ≥ 50, so the proportional share wins.
        assert_eq!(tree_width(200), 60);
        // 30% of 160 is 48 < 50, so the minimum floor kicks in.
        assert_eq!(tree_width(160), 50);
        // The tree never eats the whole area: the right pane keeps ≥ 1 col.
        assert!(tree_width(u16::MAX) <= u16::MAX - 1);

        let ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        let backend = ratatui::backend::TestBackend::new(160, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 160, 30), &ctx).unwrap()).unwrap();
        assert_eq!(
            pane.tree_inner.width, 48,
            "50-col tree pane minus its 2 border columns: {:?}",
            pane.tree_inner
        );
        assert!(pane.items_inner.x >= pane.tree_inner.width, "right pane starts after the tree");
    }


    /// Enter opens the right-click context menu on a folder AND on a song
    /// (parity with the Playlists pane), while `d`/`→` still open/play:
    /// `d` on a song issues the play query and never opens a modal.
    #[test]
    fn enter_opens_context_menu_d_still_plays() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        // `app_rx` is read below via try_recv — do not forget it; the other
        // receivers are unused, so forget them (leaking keeps the channels
        // open so sends from the fixture never error).
        std::mem::forget(work_rx);
        let _client_rx = client_rx;
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx.clone(), crossbeam::channel::unbounded().1),
            (work_tx, crossbeam::channel::unbounded().1),
            (client_tx, crossbeam::channel::unbounded().1),
        );
        let mut pane = DirectoriesPane::new(&ctx);
        pane.items = vec![dir("Folder", "Folder"), song("a.flac")];

        // Enter on the folder opens its whole-subtree context menu.
        pane.item_list.select(Some(0));
        let mut ev = action(vec![Actions::Common(CommonAction::Confirm)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(
            matches!(
                app_rx.try_recv(),
                Ok(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_)))
            ),
            "Enter on a folder opens the context menu"
        );

        // Enter on the song opens the song's context menu.
        pane.item_list.select(Some(1));
        let mut ev = action(vec![Actions::Common(CommonAction::Confirm)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(
            matches!(
                app_rx.try_recv(),
                Ok(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_)))
            ),
            "Enter on a song opens the context menu"
        );

        // `d` on the song still plays it (a play query, no modal).
        let mut ev = action(vec![Actions::Directories(DirectoriesActions::PlayFile)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(
            matches!(app_rx.try_recv(), Err(crossbeam::channel::TryRecvError::Empty)),
            "d on a song must not open a modal"
        );
    }

    #[test]
    fn esc_with_a_selection_consumes_the_keypress() {
        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.items = vec![song("a.flac"), song("b.flac"), song("c.flac")];
        pane.item_list.select(Some(0));
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![0, 1]);

        // Esc carries both Close (menu) and ShowSettings; clearing the
        // selection must consume the keypress so settings does not open on
        // the same press.
        let mut ev = action(vec![
            Actions::Common(CommonAction::Close),
            Actions::Global(GlobalAction::ShowSettings),
        ]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.marked.is_empty(), "Esc clears the selection");
        assert!(ev.is_consumed(), "clearing the selection consumes the keypress");
    }

    #[test]
    fn esc_without_a_selection_leaves_settings_enabled() {
        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.items = vec![song("a.flac"), song("b.flac")];
        pane.item_list.select(Some(0));
        let mut ev = action(vec![
            Actions::Common(CommonAction::Close),
            Actions::Global(GlobalAction::ShowSettings),
        ]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.marked.is_empty());
        assert!(!ev.is_consumed(), "no selection: Esc still opens settings");
    }

    #[test]
    fn shift_range_reanchors_after_esc_clears_the_selection() {
        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.items =
            vec![song("a.flac"), song("b.flac"), song("c.flac"), song("d.flac"), song("e.flac")];
        pane.item_list.select(Some(0));

        // Shift+Down twice marks [0..=2] (anchor 0).
        for _ in 0..2 {
            let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
            pane.handle_action(&mut ev, &mut ctx).unwrap();
        }
        assert_eq!(pane.item_list.selected(), Some(2));
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![0, 1, 2]);

        // Esc clears the marks - and (with the fix) the anchor too.
        let mut ev = action(vec![Actions::Common(CommonAction::Close)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.marked.is_empty());

        // Move the cursor back up to row 1, then Shift+Down: the range
        // must start from the new cursor (1), not reach back to the old
        // anchor (0).
        let mut ev = action(vec![Actions::Common(CommonAction::Up)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.item_list.selected(), Some(1));
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.item_list.selected(), Some(2));
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![1, 2]);
    }

    // ── Round 28: the Search tab folded into the MPD tab ──────────────

    /// The toggle row marks the active mode with the app's ●/⭘ convention
    /// and the tab starts in Library mode.
    #[test]
    fn toggle_row_marks_the_active_mode() {
        let ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        assert_eq!(pane.mode, MpdTabMode::Library, "the MPD tab starts in Library mode");

        let render_toggle_line = |pane: &mut DirectoriesPane| -> String {
            let backend = ratatui::backend::TestBackend::new(80, 20);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 20), &ctx).unwrap())
                .unwrap();
            let buf = terminal.backend().buffer();
            (0..80u16).map(|x| buf[(x, 0)].symbol().to_string()).collect()
        };

        let line = render_toggle_line(&mut pane);
        assert!(line.contains("● Library"), "Library mode: {line}");
        assert!(line.contains("⭘ Search"), "Library mode: {line}");
        assert!(!line.contains("⭘ Library"), "Library is active: {line}");
        assert!(!line.contains("● Search"), "Search is inactive: {line}");

        pane.set_mode(MpdTabMode::Search, &ctx).unwrap();
        let line = render_toggle_line(&mut pane);
        assert!(line.contains("⭘ Library"), "Search mode: {line}");
        assert!(line.contains("● Search"), "Search mode: {line}");
    }

    /// Clicking the toggle labels switches the MPD tab's mode.
    #[test]
    fn clicking_the_toggle_labels_switches_the_mode() {
        let ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 20), &ctx).unwrap()).unwrap();
        let [library_area, search_area] = pane.toggle_areas;
        assert!(search_area.width > 0, "the toggle labels are clickable");

        pane.handle_mouse_event(
            MouseEvent {
                x: search_area.x + 1,
                y: search_area.y,
                kind: MouseEventKind::LeftClick,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(pane.mode, MpdTabMode::Search, "clicking Search switches to it");

        pane.handle_mouse_event(
            MouseEvent {
                x: library_area.x + 1,
                y: library_area.y,
                kind: MouseEventKind::LeftClick,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(pane.mode, MpdTabMode::Library, "clicking Library switches back");
    }

    /// `Shift+Tab` (the ToggleMpdMode global) toggles the Library/Search
    /// mode — the ONLY key that does: Tab / E / Q (NextTab / PreviousTab)
    /// fall through and still cycle tabs, so the MPD tab stays reachable.
    #[test]
    fn shift_tab_toggles_the_library_search_mode() {
        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);

        let mut ev = action(vec![Actions::Global(GlobalAction::ToggleMpdMode)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.mode, MpdTabMode::Search, "Shift+Tab enters Search mode");

        let mut ev = action(vec![Actions::Global(GlobalAction::ToggleMpdMode)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.mode, MpdTabMode::Library, "a second Shift+Tab returns to Library");
    }

    /// Tab (NextTab) and Q (PreviousTab) must NOT toggle the mode — they
    /// are abandoned so the global handler cycles tabs as before.
    #[test]
    fn tab_and_q_do_not_toggle_the_mode() {
        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);

        let mut ev = action(vec![Actions::Global(GlobalAction::NextTab)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.mode, MpdTabMode::Library, "Tab still cycles tabs, never toggles");

        let mut ev = action(vec![Actions::Global(GlobalAction::PreviousTab)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.mode, MpdTabMode::Library, "Q still cycles tabs, never toggles");
    }

    /// In Search mode the tab renders the folded-in search UI (filters
    /// pane, results list, info box) and search results delivered to the
    /// Directories pane reach the embedded search.
    #[test]
    fn search_mode_renders_the_folded_in_search_ui() {
        let ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.set_mode(MpdTabMode::Search, &ctx).unwrap();

        pane.on_query_finished(
            crate::ui::panes::search::SEARCH,
            MpdQueryResult::SearchResult {
                data: vec![crate::mpd::commands::Song {
                    file: "/mnt/music/ambient.flac".to_owned(),
                    ..Default::default()
                }],
            },
            true,
            &ctx,
        )
        .unwrap();

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 30), &ctx).unwrap()).unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..30u16)
            .map(|y| (0..100u16).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(" Search "), "the filter pane title shows: {text}");
        assert!(text.contains(" Results "), "the results title shows: {text}");
        assert!(text.contains(" Info "), "the info box shows: {text}");
        assert!(text.contains("ambient.flac"), "the delivered result renders: {text}");
    }


}

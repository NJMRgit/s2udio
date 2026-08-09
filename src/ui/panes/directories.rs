use std::collections::{HashMap, HashSet};

use anyhow::Result;
use itertools::Itertools;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use super::Pane;
use crate::{
    MpdQueryResult,
    config::{
        keys::{CommonAction, DirectoriesActions},
        tabs::PaneType,
    },
    ctx::Ctx,
    mpd::{
        client::Client,
        commands::Song,
        mpd_client::{Filter, FilterKind, MpdClient, Tag},
    },
    shared::{
        keys::ActionEvent,
        macros::modal,
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::{Enqueue, MpdClientExt},
    },
    ui::{
        UiEvent,
        dir_or_song::DirOrSong,
        dirstack::{DirStackItem, MarkState, Path},
        modals::{
            input_modal::InputModal,
            menu::modal::MenuModal,
            select_modal::SelectModal,
        },
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
pub(crate) fn tree_width(total: u16) -> u16 {
    if total <= 120 {
        return 0;
    }
    let by_percent = (u32::from(total) * 30 / 100) as u16;
    by_percent.max(50).min(total - 1)
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
            metadata.insert(
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
        Self { name, path, display: None, children: Vec::new(), expanded: false }
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

    #[cfg(test)]
    fn toggle(&mut self, path: &[String]) {
        if let Some(node) = self.root.find_mut(path) {
            node.expanded = !node.expanded;
        }
    }

    #[cfg(test)]
    fn find(&self, path: &[String]) -> Option<&TreeNode> {
        let mut node = &self.root;
        for seg in path {
            let i = node.children.iter().position(|c| &c.name == seg)?;
            node = &node.children[i];
        }
        Some(node)
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

    #[cfg(test)]
    fn is_expanded(&self, path: &[String]) -> bool {
        self.find(path).map(|n| n.expanded).unwrap_or(false)
    }

    /// A bottom directory: no subdirectories to expand.
    #[cfg(test)]
    fn is_leaf(&self, path: &[String]) -> bool {
        self.find(path).map(|n| n.children.is_empty()).unwrap_or(false)
    }
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
    /// Children lists keyed by path, so backing out never refetches.
    loaded: HashMap<Path, Vec<DirOrSong>>,
    /// Paths with a fetch in flight (avoid duplicate requests).
    pending: HashSet<Path>,
    /// Queue id of a file played via `PlayFile` (`d`/`→`); dropped on song
    /// change / stop — mirrors the Radio pane.
    temp_play_id: Option<u32>,
    initialized: bool,
}

impl DirectoriesPane {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            tree: DirTree::default(),
            tree_state: ListState::default(),
            tree_inner: Rect::default(),
            items: Vec::new(),
            item_list: ListState::default(),
            items_inner: Rect::default(),
            marked: MarkState::default(),
            selected: None,
            loaded: HashMap::new(),
            pending: HashSet::new(),
            temp_play_id: None,
            initialized: false,
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
            .target(PaneType::Directories)
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
                    .sorted_by(|a, b| a.with_custom_sort(&sort).cmp(&b.with_custom_sort(&sort)))
                    .collect();
                Ok(MpdQueryResult::DirOrSong { data, path: Some(path) })
            });
    }

    /// Populate the right pane for the current node: always its children
    /// (folders + songs one level deep). At the root (`selected` = None)
    /// the right pane lists every top-level directory. Playlists and
    /// hidden directories never display here.
    fn populate_items(&mut self) {
        self.item_list.select(None);
        // A fresh children list has no multi-selection (marks belong to
        // the list that was on screen).
        self.marked.clear();
        let mut items: Vec<DirOrSong> = match self.selected.as_ref() {
            Some(path) => self.loaded.get(path).cloned().unwrap_or_default(),
            None => self.loaded.get(&Path::new()).cloned().unwrap_or_default(),
        }
        .into_iter()
        .filter(is_visible_entry)
        .collect();
        if self.selected.is_none() {
            // The downloads folder (~/Downloads/s2udio-downloads, outside
            // the MPD library) is listed at the top of the library root,
            // shown as "Downloads".
            items.insert(
                0,
                DirOrSong::Dir {
                    name: "Downloads".to_owned(),
                    full_path: crate::ui::modals::paste::DOWNLOADS_DIR_NAME.to_owned(),
                    last_modified: chrono::Utc::now(),
                    playlist: false,
                },
            );
        }
        self.items = items;
        if !self.items.is_empty() {
            self.item_list.select(Some(0));
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
                .and_then(|selected| flat.iter().position(|(n, _)| n.path == selected.as_slice()));
            by_item.or(by_node)
        };
        if let Some(idx) = idx {
            self.tree.selected = idx;
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

    /// Back out one level: the right pane shows the parent's children (with
    /// the row we came from highlighted), the tree highlight follows and
    /// the branch we left collapses. At the root this is a no-op.
    fn select_parent(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(current) = self.selected.clone() else { return Ok(()) };
        if current.is_empty() {
            return Ok(());
        }
        let prev = current.clone();
        // Moving up collapses the branch we leave: the tree shows the path
        // up to the current node, not the whole history of expansions.
        self.tree.collapse(prev.as_slice());
        let mut parent = current;
        parent.pop();
        self.selected = if parent.is_empty() { None } else { Some(parent.clone()) };
        self.fetch_children(&parent, ctx);
        self.populate_items();
        self.select_items_item(&prev);
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

    /// Highlight a tree row and mirror it in the right pane: the current
    /// node becomes the highlighted row and the right pane shows its
    /// children (the Library root shows the top-level list).
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

    /// Move the tree highlight; the highlighted folder's children fill the
    /// right pane.
    fn move_tree(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = {
            let mut flat = Vec::new();
            self.tree.visible(&self.tree.root, 0, &mut flat);
            flat.len()
        };
        if len == 0 {
            return Ok(());
        }
        let current = i64::try_from(self.tree.selected.min(len - 1)).unwrap_or(0);
        let new_idx = (current + dir).clamp(0, i64::try_from(len - 1).unwrap_or(0)) as usize;
        if new_idx != self.tree.selected {
            self.highlight_tree_node(new_idx, ctx)?;
        }
        Ok(())
    }

    /// Move the right-pane cursor; the left tree follows when the item has
    /// a tree row (folders).
    fn move_items(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        if self.items.is_empty() {
            return Ok(());
        }
        let current = i64::try_from(self.item_list.selected().unwrap_or(0)).unwrap_or(0);
        let new_idx = (current + dir).clamp(0, i64::try_from(self.items.len() - 1).unwrap_or(0))
            as usize;
        if new_idx != current as usize {
            self.item_list.select(Some(new_idx));
            self.sync_tree_to_items_cursor();
            ctx.render()?;
        }
        Ok(())
    }

    /// Context menu for a tree folder: add the whole subtree to the queue
    /// or to a playlist.
    fn open_folder_menu(&mut self, path: &Path, ctx: &Ctx) -> Result<()> {
        let path_str = path.to_string();
        let folder_name = path
            .as_slice()
            .last()
            .cloned()
            .unwrap_or_else(|| "Library".to_owned());
        // Every song under the folder, recursively.
        let find_songs = move |client: &mut Client<'_>| -> Result<Vec<Song>> {
            Ok(client.find(&[Filter::new_with_kind(Tag::File, &path_str, FilterKind::StartsWith)])?)
        };

        let modal = MenuModal::new(ctx)
            .list_section(ctx, |mut section| {
                let find = find_songs.clone();
                section.add_item("Add folder to queue", move |ctx| {
                    ctx.command(move |client| {
                        let songs = find(client)?;
                        let items: Vec<Enqueue> =
                            songs.into_iter().map(|s| Enqueue::File { path: s.file }).collect();
                        client.enqueue_multiple(items, None, None, false)?;
                        Ok(())
                    });
                    Ok(())
                });
                let find = find_songs.clone();
                section.add_item("Replace queue with folder", move |ctx| {
                    ctx.command(move |client| {
                        let songs = find(client)?;
                        let items: Vec<Enqueue> =
                            songs.into_iter().map(|s| Enqueue::File { path: s.file }).collect();
                        client.enqueue_multiple(items, None, None, true)?;
                        Ok(())
                    });
                    Ok(())
                });
                Some(section)
            })
            .list_section(ctx, |mut section| {
                let find = find_songs.clone();
                let initial = folder_name.clone();
                section.add_item("Create playlist from folder", move |ctx| {
                    modal!(
                        ctx,
                        InputModal::new(ctx)
                            .title("Create new playlist")
                            .confirm_label("Save")
                            .input_label("Playlist name:")
                            .initial_value(initial)
                            .on_confirm(move |ctx, value| {
                                let value = value.to_owned();
                                let find = find.clone();
                                ctx.command(move |client| {
                                    let songs = find(client)?;
                                    client.create_playlist(
                                        &value,
                                        songs.into_iter().map(|s| s.file).collect(),
                                    )?;
                                    Ok(())
                                });
                                Ok(())
                            })
                    );
                    Ok(())
                });
                let find = find_songs.clone();
                section.add_item("Add folder to playlist", move |ctx| {
                    // The radio favourites playlist is Radio-tab-owned: it
                    // never appears as an add target.
                    let radio_playlist = ctx.config.radio.playlist.clone();
                    let (items, playlists) = ctx.query_sync(move |client| {
                        let songs = find(client)?;
                        let playlists = client
                            .picker_playlists(&radio_playlist)?
                            .into_iter()
                            .map(|p| p.name)
                            .collect_vec();
                        Ok((songs, playlists))
                    })?;
                    modal!(
                        ctx,
                        SelectModal::builder()
                            .ctx(ctx)
                            .options(playlists)
                            .confirm_label("Add")
                            .title("Select a playlist")
                            .on_confirm(move |ctx, selected, _idx| {
                                ctx.command(move |client| {
                                    client.add_to_playlist_multiple(
                                        &selected,
                                        items.into_iter().map(|s| s.file).collect_vec(),
                                    )?;
                                    Ok(())
                                });
                                Ok(())
                            })
                            .build()
                    );
                    Ok(())
                });
                Some(section)
            });

        crate::shared::macros::modal!(ctx, modal);
        Ok(())
    }

    /// Context menu for a highlighted file: add it to the queue or to a
    /// playlist. When songs are marked, the menu acts on every marked
    /// song (like the audio queue list's menu); otherwise the highlighted
    /// file.
    fn open_song_menu(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.selected_item() else { return Ok(()) };
        // The songs the menu acts on: the marked songs when any are
        // marked, else the highlighted file.
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
        let current_items: Vec<Enqueue> =
            songs.iter().map(|s| Enqueue::File { path: s.file.clone() }).collect();
        let list_songs = move |_client: &mut Client<'_>| -> Result<Vec<Song>> {
            Ok(songs.clone())
        };

        let modal = MenuModal::new(ctx)
            .list_section(ctx, |mut section| {
                if !current_items.is_empty() {
                    let cloned_items = current_items.clone();
                    section.add_item("Add to queue", move |ctx| {
                        ctx.command(move |client| {
                            client.enqueue_multiple(cloned_items, None, None, false)?;
                            Ok(())
                        });
                        Ok(())
                    });
                    let cloned_items = current_items.clone();
                    section.add_item("Replace queue", move |ctx| {
                        ctx.command(move |client| {
                            client.enqueue_multiple(cloned_items, None, None, true)?;
                            Ok(())
                        });
                        Ok(())
                    });
                }

                let songs_in_item = list_songs.clone();
                section.add_item("Create playlist", move |ctx| {
                    modal!(
                        ctx,
                        InputModal::new(ctx)
                            .title("Create new playlist")
                            .confirm_label("Save")
                            .input_label("Playlist name:")
                            .on_confirm(move |ctx, value| {
                                let value = value.to_owned();
                                ctx.command(move |client| {
                                    let songs = songs_in_item(client)?;
                                    client.create_playlist(
                                        &value,
                                        songs.into_iter().map(|s| s.file).collect(),
                                    )?;
                                    Ok(())
                                });
                                Ok(())
                            })
                    );
                    Ok(())
                });

                let songs_in_item = list_songs.clone();
                section.add_item("Add to playlist", move |ctx| {
                    // The radio favourites playlist is Radio-tab-owned: it
                    // never appears as an add target.
                    let radio_playlist = ctx.config.radio.playlist.clone();
                    let (items, playlists) = ctx.query_sync(move |client| {
                        let songs = songs_in_item(client)?;
                        let playlists = client
                            .picker_playlists(&radio_playlist)?
                            .into_iter()
                            .map(|p| p.name)
                            .collect_vec();
                        Ok((songs, playlists))
                    })?;
                    modal!(
                        ctx,
                        SelectModal::builder()
                            .ctx(ctx)
                            .options(playlists)
                            .confirm_label("Add")
                            .title("Select a playlist")
                            .on_confirm(move |ctx, selected, _idx| {
                                ctx.command(move |client| {
                                    client.add_to_playlist_multiple(
                                        &selected,
                                        items.into_iter().map(|s| s.file).collect_vec(),
                                    )?;
                                    Ok(())
                                });
                                Ok(())
                            })
                            .build()
                    );
                    Ok(())
                });
                Some(section)
            })
            .list_section(ctx, |section| {
                let section = section.item("Cancel", |_ctx| Ok(()));
                Some(section)
            })
            .build();

        modal!(ctx, modal);
        Ok(())
    }

    fn selected_item(&self) -> Option<DirOrSong> {
        let idx = self.item_list.selected()?;
        self.items.get(idx).cloned()
    }

    /// Right arrow / `d` (or double-click on a file): play the
    /// highlighted file immediately without adding it to the queue (it is
    /// removed again once the song changes).
    fn play_selected_file(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(DirOrSong::Song(song)) = self.selected_item() else {
            return Ok(());
        };
        // Drop any previous temporary play entry first, so repeatedly
        // playing files never grows the queue (the SongChanged cleanup below
        // only fires when the song actually moves on, which can lag or miss
        // consecutive plays).
        if let Some(prev) = self.temp_play_id.take() {
            ctx.temp_play_id.set(None);
            ctx.command(move |client| {
                client.delete_id(prev)?;
                Ok(())
            });
        }
        let file = song.file.clone();
        // The Downloads folder lives outside the MPD library (MPD cannot
        // play files from it): play the file through mpv instead — mpv
        // handles both audio and video, like torrent streams.
        if crate::ui::modals::paste::downloads_dir().is_some_and(|dir| {
            std::path::Path::new(&file).starts_with(&dir)
        }) {
            let title = std::path::Path::new(&file)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.clone());
            crate::core::mpv::play_video_entries(
                ctx,
                vec![crate::core::mpv::MpvPlaylistEntry::new(title, file, None)],
            );
            return Ok(());
        }
        ctx.query().id(PLAY_FILE).replace_id(PLAY_FILE).target(PaneType::Directories).query(
            move |client| {
                let id = client.add_id(&file, None)?;
                client.play_id(id)?;
                Ok(MpdQueryResult::Any(Box::new(id)))
            },
        );
        Ok(())
    }

    /// Drop the temporary play song once playback has moved on.
    fn cleanup_temp_play(&mut self, ctx: &Ctx) {
        if let Some(temp) = self.temp_play_id
            && ctx.status.songid != Some(temp)
        {
            self.temp_play_id = None;
            ctx.temp_play_id.set(None);
            ctx.command(move |client| {
                client.delete_id(temp)?;
                Ok(())
            });
        }
    }

    fn render_tree(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let mut flat = Vec::new();
        self.tree.visible(&self.tree.root, 0, &mut flat);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(" Folders ");
        let inner = block.inner(area);
        self.tree_inner = inner;

        // The row under the mouse gets the hover highlight (slightly
        // brighter than the keyboard selection, dimmer than marked rows).
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            inner,
            self.tree_state.offset(),
            flat.len(),
            1,
        );
        let hovered_style = ctx.config.theme.hovered_item_style;
        let items: Vec<ListItem> = flat
            .iter()
            .enumerate()
            .map(|(idx, (node, depth))| {
                // The Library root is never collapsible: no arrow, and the
                // ↴ glyph marks the always-open entry point.
                if node.path.is_empty() {
                    return ListItem::new(Line::from(Span::raw("Library ↴")));
                }
                // Only folders with subdirectories get an arrow; leaves keep
                // a spacer so the names stay aligned.
                let arrow = if node.children.is_empty() {
                    "  "
                } else if node.expanded {
                    "▾ "
                } else {
                    "▸ "
                };
                let indent = "  ".repeat(*depth);
                let mut item = ListItem::new(Line::from(Span::raw(format!(
                    "{indent}{arrow}{}",
                    node.display_name()
                ))));
                if hover_idx == Some(idx) {
                    item = item.style(hovered_style);
                }
                item
            })
            .collect();

        self.tree_state.select(Some(self.tree.selected.min(flat.len().saturating_sub(1))));
        ratatui::widgets::StatefulWidget::render(
            List::new(items)
                .highlight_style(if hover_idx == self.tree_state.selected() {
                    ctx.config.theme.hovered_item_style
                } else {
                    ctx.config.theme.current_item_style
                })
                .style(ctx.config.as_list_name_style()),
            inner,
            frame.buffer_mut(),
            &mut self.tree_state,
        );
        ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
    }

    fn render_items(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        // The MPD browser rows: directories get a ▶ prefix, songs get none
        // (no D/S markers); multi-selected rows render with the lighter
        // marked highlight (like the queue list), the row under the mouse
        // with the hover highlight.
        let title = match self.selected.as_ref() {
            Some(path) => path.current_dir().map_or_else(|| "Library".to_owned(), |name| {
                if name == crate::ui::modals::paste::DOWNLOADS_DIR_NAME {
                    "Downloads".to_owned()
                } else {
                    name.to_owned()
                }
            }),
            None => "Library".to_owned(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(format!(" {title}({}) ", self.items.len()));
        let inner = block.inner(area);
        self.items_inner = inner;

        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            inner,
            self.item_list.offset(),
            self.items.len(),
            1,
        );
        let hovered_style = ctx.config.theme.hovered_item_style;
        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let is_marked = self.marked.contains(idx);
                let item = match item {
                    DirOrSong::Dir { name, .. } => ListItem::from(Line::from(vec![
                        Span::from("▶ "),
                        Span::from(if name.is_empty() {
                            "Untitled".to_owned()
                        } else {
                            name.clone()
                        }),
                    ])),
                    DirOrSong::Song(song) => {
                        let spans: Vec<Span> = ctx
                            .config
                            .theme
                            .browser_song_format
                            .0
                            .iter()
                            .map(|prop| {
                                Span::from(
                                    prop.as_string(
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
                } else if hover_idx == Some(idx) {
                    item.style(hovered_style)
                } else {
                    item
                }
            })
            .collect();

        ratatui::widgets::StatefulWidget::render(
            List::new(items)
                .highlight_style(if hover_idx == self.item_list.selected() {
                    ctx.config.theme.hovered_item_style
                } else {
                    ctx.config.theme.current_item_style
                })
                .style(ctx.config.as_list_name_style()),
            inner,
            frame.buffer_mut(),
            &mut self.item_list,
        );
        ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
    }

    fn render_tips(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        let tips = vec![
            Line::from(vec![
                Span::styled("w/s · ↑/↓", base),
                Span::styled("  folders · items", dim),
            ]),
            Line::from(vec![
                Span::styled("d / a", base),
                Span::styled("  open · back out", dim),
            ]),
            Line::from(vec![
                Span::styled("Enter", base),
                Span::styled("  context menu", dim),
            ]),
            Line::from(vec![
                Span::styled("d / →", base),
                Span::styled("  open · play", dim),
            ]),
        ];
        frame.render_widget(Paragraph::new(tips).style(dim), area);
    }

    fn render_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let mut items: Vec<ListItem> = Vec::new();
        if let Some(selected) = self.selected_item() {
            match selected {
                DirOrSong::Song(song) => {
                    for group in song.to_file_preview(ctx) {
                        if let Some(name) = group.name {
                            items.push(ListItem::new(Line::styled(
                                name,
                                group.header_style.unwrap_or_default(),
                            )));
                        }
                        items.extend(group.items);
                        items.push(ListItem::new(""));
                    }
                }
                DirOrSong::Dir { name, full_path, last_modified, .. } => {
                    let key = ctx.config.theme.preview_label_style;
                    let group = ctx.config.theme.preview_metadata_group_style;
                    items.push(ListItem::new(Line::styled(" --- [Folder]", group)));
                    items.push(ListItem::new(Line::from(vec![
                        Span::styled("Name", key),
                        Span::raw(": "),
                        Span::raw(name.clone()),
                    ])));
                    items.push(ListItem::new(Line::from(vec![
                        Span::styled("Path", key),
                        Span::raw(": "),
                        Span::raw(full_path.clone()),
                    ])));
                    items.push(ListItem::new(Line::from(vec![
                        Span::styled("Last Modified", key),
                        Span::raw(": "),
                        Span::raw(last_modified.to_string()),
                    ])));
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

    fn handle_tree_mouse(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let row = usize::from(event.y.saturating_sub(self.tree_inner.y)) + self.tree_state.offset();
        match event.kind {
            MouseEventKind::LeftClick => self.highlight_tree_node(row, ctx),
            MouseEventKind::DoubleClick => {
                // Double-click selects the folder and toggles it: a folder
                // with subdirectories expands/collapses, a bottom directory
                // just opens (its files fill the right pane).
                self.highlight_tree_node(row, ctx)?;
                let toggle: Option<(Path, bool)> = {
                    let mut flat = Vec::new();
                    self.tree.visible(&self.tree.root, 0, &mut flat);
                    flat.get(row)
                        .filter(|(node, _)| !node.path.is_empty() && !node.children.is_empty())
                        .map(|(node, _)| (Path::from(node.path.clone()), node.expanded))
                };
                if let Some((path, expanded)) = toggle {
                    self.set_expanded(&path, !expanded, ctx)?;
                }
                Ok(())
            }
            MouseEventKind::RightClick => {
                // Context menu for the clicked folder (or the Library root).
                let path = {
                    let mut flat = Vec::new();
                    self.tree.visible(&self.tree.root, 0, &mut flat);
                    flat.get(row).map(|(node, _)| Path::from(node.path.clone()))
                };
                if let Some(path) = path {
                    self.open_folder_menu(&path, ctx)
                } else {
                    Ok(())
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                self.move_tree(dir, ctx)
            }
            _ => Ok(()),
        }
    }

    fn handle_items_mouse(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let row = usize::from(event.y.saturating_sub(self.items_inner.y)) + self.item_list.offset();
        match event.kind {
            MouseEventKind::DoubleClick => {
                if row < self.items.len() {
                    self.item_list.select(Some(row));
                    // Keep the tree highlight in sync with the click
                    // selection (like the keyboard path).
                    self.sync_tree_to_items_cursor();
                    let item = self.items[row].clone();
                    if item.is_file() {
                        self.play_selected_file(ctx)?;
                    } else {
                        self.open_item(item, ctx)?;
                    }
                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick
                if event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                if row < self.items.len() {
                    self.item_list.select(Some(row));
                    self.marked.toggle(row);
                    self.sync_tree_to_items_cursor();
                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick
                if event.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
            {
                if row < self.items.len() {
                    if self.marked.anchor().is_none() {
                        self.marked.set_anchor(row);
                    }
                    // Replace the previous alt/shift range, so alt+clicking
                    // closer to the anchor deselects the items beyond it.
                    self.marked.select_range(row);
                    self.item_list.select(Some(row));
                    self.sync_tree_to_items_cursor();
                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick => {
                if row < self.items.len() {
                    // A plain click on a different row drops the
                    // multi-selection; clicking the selected row keeps it.
                    if !self.marked.is_empty()
                        && Some(row) != self.item_list.selected()
                    {
                        self.marked.clear();
                    }
                    self.item_list.select(Some(row));
                    self.marked.set_anchor(row);
                    self.marked.clear_range();
                    // Keep the tree highlight in sync with the click
                    // selection (like the keyboard path).
                    self.sync_tree_to_items_cursor();
                    ctx.render()?;
                }
            }
            MouseEventKind::RightClick => {
                if row < self.items.len() {
                    self.item_list.select(Some(row));
                    self.sync_tree_to_items_cursor();
                    ctx.render()?;
                    let item = self.items[row].clone();
                    if item.is_file() {
                        return self.open_song_menu(ctx);
                    }
                    let DirOrSong::Dir { full_path, .. } = item else { return Ok(()) };
                    let path = split_path(&full_path);
                    return self.open_folder_menu(&path, ctx);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                self.move_items(dir, ctx)?;
            }
            _ => {}
        }
        Ok(())
    }
}

impl Pane for DirectoriesPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        // The left folder-tree pane keeps a 50-column minimum and is
        // hidden entirely on TUIs ≤ 120 columns wide (the right pane
        // then gets the whole area).
        let tree_w = tree_width(area.width);
        let (tree_area, right) = if tree_w == 0 {
            (Rect::default(), area)
        } else {
            let [tree_area, right] = Layout::horizontal([
                Constraint::Length(tree_w),
                Constraint::Length(area.width - tree_w),
            ])
            .areas(area);
            (tree_area, right)
        };
        // The info box takes about two thirds of the pane height (the tips
        // strip stays a fixed 3 rows); the item list gets the rest. Exact
        // lengths are computed so the rows always fill the area exactly.
        let tips_h = 3;
        let info_h = (right.height.saturating_sub(tips_h) * 2 / 3).min(15);
        let files_h = right.height.saturating_sub(tips_h + info_h);
        let [files_area, tips_area, info_area] = Layout::vertical([
            Constraint::Length(files_h),
            Constraint::Length(tips_h),
            Constraint::Length(info_h),
        ])
        .areas(right);

        if tree_w > 0 {
            self.render_tree(frame, tree_area, ctx);
        }
        self.render_items(frame, files_area, ctx);
        self.render_tips(frame, tips_area, ctx);
        self.render_info(frame, info_area, ctx);
        Ok(())
    }

    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        if !self.initialized {
            self.fetch_children(&Path::new(), ctx);
            ctx.query().id(TREE).replace_id(TREE).target(PaneType::Directories).query(
                move |client| {
                    let dirs: Vec<String> = client
                        .list_all(None)?
                        .into_iter()
                        .filter_map(|e| match e {
                            crate::mpd::commands::list_all::ListAllEntry::Dir(p) => Some(p),
                            _ => None,
                        })
                        .collect();
                    Ok(MpdQueryResult::Any(Box::new(dirs)))
                },
            );
            self.initialized = true;
        }

        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, _is_visible: bool, ctx: &Ctx) -> Result<()> {
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
            UiEvent::Reconnected => {
                self.initialized = false;
                self.before_show(ctx)?;
                self.temp_play_id = None;
            }
            UiEvent::SongChanged => {
                // Remove the temporary play song from the queue once
                // playback has moved on.
                self.cleanup_temp_play(ctx);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if self.tree_inner.contains(event.into()) {
            return self.handle_tree_mouse(event, ctx);
        }
        if self.items_inner.contains(event.into()) {
            return self.handle_items_mouse(event, ctx);
        }
        Ok(())
    }

    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        // Common actions come first: `w/s`/arrows drive the right-pane list
        // (the primary navigation surface; both panes share the selection),
        // while the tree is a mirror that follows the current node.
        if let Some(action) = event.claim_common() {
            match action {
                CommonAction::Up | CommonAction::Down => {
                    let dir = if matches!(action, CommonAction::Up) { -1 } else { 1 };
                    return self.move_items(dir, ctx);
                }
                CommonAction::Left => {
                    // `←` backs out one level (same as `a`).
                    return self.select_parent(ctx);
                }
                CommonAction::Top => {
                    if !self.items.is_empty() {
                        self.item_list.select(Some(0));
                        self.sync_tree_to_items_cursor();
                        ctx.render()?;
                    }
                    return Ok(());
                }
                CommonAction::Bottom => {
                    if !self.items.is_empty() {
                        self.item_list.select(Some(self.items.len() - 1));
                        self.sync_tree_to_items_cursor();
                        ctx.render()?;
                    }
                    return Ok(());
                }
                CommonAction::SelectUp | CommonAction::SelectDown => {
                    // Shift+Up/Down: range-select from the anchor (set by
                    // plain clicks / the first shift-press), moving first so
                    // the newly reached row is included; each press
                    // replaces the previous range.
                    let dir = if matches!(action, CommonAction::SelectDown) { 1 } else { -1 };
                    let start = self.item_list.selected().unwrap_or(0);
                    if self.marked.anchor().is_none() {
                        self.marked.set_anchor(start);
                    }
                    self.move_items(dir, ctx)?;
                    let sel = self.item_list.selected().unwrap_or(start);
                    self.marked.select_range(sel);
                    ctx.render()?;
                    return Ok(());
                }
                // Enter opens the context menu (like right-click), same as
                // the Playlists pane; `d`/`→` keep open/play below.
                CommonAction::Confirm => return self.open_context_menu(ctx),
                CommonAction::ContextMenu => return self.open_context_menu(ctx),
                _ => event.abandon(),
            }
        }
        if let Some(action) = event.claim_directories() {
            return match action {
                DirectoriesActions::FolderUp | DirectoriesActions::FolderDown => {
                    // Normally claimed by Common Up/Down above; keep a
                    // tree fallback for other bindings.
                    let dir = if matches!(action, DirectoriesActions::FolderUp) { -1 } else { 1 };
                    self.move_tree(dir, ctx)
                }
                // `d` mirrors `→` (wasd = arrow keys): open the highlighted
                // folder or play the highlighted file.
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    let Some(item) = self.selected_item() else { return Ok(()) };
                    if item.is_file() {
                        self.play_selected_file(ctx)
                    } else {
                        self.open_item(item, ctx)
                    }
                }
                DirectoriesActions::FolderCollapse => {
                    // `a` / `←`: back out one level (parent's children in
                    // the right pane). At the root this is a no-op.
                    self.select_parent(ctx)
                }
            };
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
            (FETCH_DATA, MpdQueryResult::DirOrSong { data, path }) => {
                let Some(path) = path else {
                    log::error!(path:?; "Cannot insert data because path is not provided");
                    return Ok(());
                };
                // Only the last fetch of the (replace_id) identity ever
                // runs: the client task skips superseded queries without
                // firing their callbacks, so no other pending path can
                // still deliver a result. Drop them all to keep `pending`
                // honest — a superseded dir must fetch again on re-entry
                // instead of silently showing an empty pane.
                self.pending.clear();
                self.loaded.insert(path.clone(), data);
                // Only the current node's list (or the root list while at
                // the root) is shown; stale fetches are dropped.
                let is_current = self.selected.as_ref() == Some(&path)
                    || (self.selected.is_none() && path.is_empty());
                if is_current {
                    self.populate_items();
                    self.sync_tree_to_items_cursor();
                    ctx.render()?;
                }
            }
            (TREE, MpdQueryResult::Any(any)) => {
                if let Ok(dirs) = any.downcast::<Vec<String>>() {
                    self.tree = DirTree::build(dirs.into_iter());
                    // The downloads folder (~/Downloads/s2udio-downloads,
                    // outside the MPD library) sits at the top of the
                    // library, right under "Library ↴", displayed as
                    // "Downloads" (hidden dirs are otherwise skipped by
                    // the build).
                    self.tree.root.children.insert(
                        0,
                        TreeNode {
                            name: crate::ui::modals::paste::DOWNLOADS_DIR_NAME.to_owned(),
                            path: vec![crate::ui::modals::paste::DOWNLOADS_DIR_NAME.to_owned()],
                            display: Some("Downloads".to_owned()),
                            children: Vec::new(),
                            expanded: false,
                        },
                    );
                    ctx.render()?;
                }
            }
            (PLAY_FILE, MpdQueryResult::Any(any)) => {
                if let Ok(id) = any.downcast::<u32>() {
                    self.temp_play_id = Some(*id);
                    // Expose the id so the Queue pane can hide the
                    // temporary entry from its list.
                    ctx.temp_play_id.set(Some(*id));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl DirectoriesPane {
    /// Context menu for the highlighted item: a folder's whole-subtree
    /// menu or a file's queue/playlist menu.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::keys::Actions;

    fn dir(name: &str, full_path: &str) -> DirOrSong {
        DirOrSong::Dir {
            name: name.to_owned(),
            full_path: full_path.to_owned(),
            last_modified: chrono::Utc::now(),
            playlist: false,
        }
    }

    fn song(file: &str) -> DirOrSong {
        DirOrSong::Song(crate::mpd::commands::Song {
            file: file.to_owned(),
            ..Default::default()
        })
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
        assert_eq!(shown, [&dir.join("a.mp4").to_string_lossy(), &dir.join("b.mp4").to_string_lossy()]);
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

    #[test]
    fn tree_build_flatten_sync() {
        let mut tree = DirTree::build(
            ["A/B".to_owned(), "A/C".to_owned(), "A/B/D".to_owned(), "Z".to_owned()].into_iter(),
        );
        assert_eq!(tree.root.children.len(), 2);
        assert_eq!(tree.root.children[0].name, "A");
        assert_eq!(tree.root.children[0].children.len(), 2);
        assert_eq!(tree.root.children[0].children[0].name, "B");
        assert_eq!(tree.root.children[0].children[0].children.len(), 1);

        // expand A: root, A, B, C, Z (B and Z need expansion for their children)
        tree.toggle(&["A".to_owned()]);
        let mut flat = Vec::new();
        tree.visible(&tree.root, 0, &mut flat);
        let names: Vec<&str> = flat.iter().map(|(n, _)| n.name.as_str()).collect();
        assert_eq!(names, ["Library", "A", "B", "C", "Z"]);

        // sync to A/B/D: path + ancestors expanded, node selected
        let mut p = Path::new();
        for s in ["A", "B", "D"] {
            p.push(s.to_owned());
        }
        tree.sync(&p);
        let mut flat1 = Vec::new();
        tree.visible(&tree.root, 0, &mut flat1);
        let names1: Vec<&str> = flat1.iter().map(|(n, _)| n.name.as_str()).collect();
        assert_eq!(names1, ["Library", "A", "B", "D", "C", "Z"]);
        assert_eq!(tree.selected, 3);

        // collapse A: its subtree hides, but A and Z stay visible
        tree.toggle(&["A".to_owned()]);
        let mut flat2 = Vec::new();
        tree.visible(&tree.root, 0, &mut flat2);
        assert_eq!(flat2.len(), 3);

        // is_leaf: "D" is a bottom directory, "A" and "B" are not.
        assert!(tree.is_leaf(&["A".to_owned(), "B".to_owned(), "D".to_owned()]));
        assert!(!tree.is_leaf(&["A".to_owned()]));
        assert!(!tree.is_leaf(&["A".to_owned(), "B".to_owned()]));
    }

    /// At the root the right pane lists every top-level directory; `d`/`→`
    /// opens the highlighted folder (tree path expanded, children shown) —
    /// highlighting alone never lists files recursively.
    #[test]
    fn root_lists_top_directories_and_open_shows_children() {
        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.tree = DirTree::build(["A/B".to_owned(), "Z".to_owned()].into_iter());
        pane.loaded.insert(Path::new(), vec![dir("A", "A"), dir("Z", "Z")]);
        pane.loaded.insert(
            Path::from(["A"]),
            vec![dir("B", "A/B"), song("A/song.flac")],
        );

        // Root: the right pane lists the top-level directories, with the
        // downloads folder shown as "Downloads" at the top.
        pane.selected = None;
        pane.populate_items();
        assert_eq!(pane.items.len(), 3, "root lists the top-level directories + Downloads");
        let DirOrSong::Dir { name, .. } = &pane.items[0] else {
            panic!("Downloads must be a folder entry");
        };
        assert_eq!(name, "Downloads");
        assert_eq!(pane.items[0].as_path(), "Downloads");
        assert_eq!(pane.items[1].as_path(), "A");
        assert_eq!(pane.items[2].as_path(), "Z");

        // Moving the cursor through the root list only moves the highlight
        // (no fetch, no recursive file list, no tree expansion).
        pane.move_items(1, &ctx).unwrap();
        assert_eq!(pane.item_list.selected(), Some(1), "cursor moved down");
        assert!(pane.selected.is_none(), "highlighting never opens a folder");
        assert_eq!(pane.items.len(), 3);
        assert!(!pane.tree.is_expanded(&["A".to_owned()]), "highlighting never expands");
        pane.move_items(-1, &ctx).unwrap();
        assert_eq!(pane.item_list.selected(), Some(0));

        // `d` on the highlighted folder opens it: children shown, path
        // expanded in the tree.
        pane.item_list.select(Some(1));
        let mut ev = action(vec![Actions::Directories(DirectoriesActions::FolderExpand)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.selected.as_ref().map(|p| p.to_string()), Some("A".to_owned()));
        assert_eq!(pane.items.len(), 2, "a folder shows its children one level deep");
        assert_eq!(pane.items[0].as_path(), "B");
        assert_eq!(pane.items[1].as_path(), "A/song.flac");
        assert!(pane.tree.is_expanded(&["A".to_owned()]), "opening expands the tree path");
    }

    /// Backing out one level shows the parent's children with the row we
    /// came from highlighted and collapses the branch we left; at the root
    /// it is a no-op.
    #[test]
    fn back_out_returns_to_the_parent_and_collapses_the_branch() {
        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.tree = DirTree::build(["A/B".to_owned(), "Z".to_owned()].into_iter());
        pane.loaded.insert(Path::new(), vec![dir("A", "A"), dir("Z", "Z")]);
        pane.loaded.insert(Path::from(["A"]), vec![dir("B", "A/B")]);
        pane.loaded.insert(Path::from(["A", "B"]), vec![song("A/B/track.flac")]);

        // Inside A/B.
        let path = Path::from(["A", "B"]);
        pane.selected = Some(path.clone());
        pane.tree.sync(&path);
        pane.populate_items();
        assert_eq!(pane.items.len(), 1);
        assert_eq!(pane.items[0].as_path(), "A/B/track.flac");

        // `a` backs out to A: A's children, cursor on B, B's branch
        // collapses, the tree follows the cursor onto B.
        let mut ev = action(vec![Actions::Directories(DirectoriesActions::FolderCollapse)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.selected.as_ref().map(|p| p.to_string()), Some("A".to_owned()));
        assert_eq!(pane.items.len(), 1);
        assert_eq!(pane.items[0].as_path(), "B", "the row we came from is highlighted");
        assert!(!pane.tree.is_expanded(&["A".to_owned(), "B".to_owned()]), "branch left collapses");
        assert!(pane.tree.is_expanded(&["A".to_owned()]), "the parent stays expanded");

        // Back out to the root: the top-level list again (Downloads + A + Z),
        // cursor on A.
        let mut ev = action(vec![Actions::Directories(DirectoriesActions::FolderCollapse)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.selected.is_none(), "backed out to the root");
        assert_eq!(pane.items.len(), 3, "root lists the top-level directories + Downloads again");
        assert_eq!(pane.items[0].as_path(), "Downloads");
        assert_eq!(pane.items[1].as_path(), "A");
        assert!(!pane.tree.is_expanded(&["A".to_owned()]), "the folder we left collapses");

        // Backing out at the root is a no-op.
        let mut ev = action(vec![Actions::Directories(DirectoriesActions::FolderCollapse)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.selected.is_none());
        assert_eq!(pane.items.len(), 3);
    }

    /// The Library root is always expanded and never collapsible.
    #[test]
    fn library_root_is_not_collapsible() {
        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.tree = DirTree::build(["A".to_owned(), "B".to_owned()].into_iter());

        let root = Path::new();
        pane.set_expanded(&root, false, &ctx).unwrap();
        assert!(pane.tree.root.expanded, "the root stays expanded");

        // Expanding a real folder works.
        pane.set_expanded(&Path::from(["A"]), true, &ctx).unwrap();
        assert!(pane.tree.is_expanded(&["A".to_owned()]));
    }

    /// The tree renders the root as `Library ↴` (no collapsible arrow) and
    /// highlights the row the cursor sits on.
    #[test]
    fn tree_renders_the_root_as_library() {
        let mut ctx = test_ctx();
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

    /// Playlists are excluded from the children list: the MPD browser
    /// lists folders and songs only.
    #[test]
    fn children_exclude_playlists() {
        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.selected = None;
        pane.loaded.insert(
            Path::new(),
            vec![
                dir("A", "A"),
                DirOrSong::Dir {
                    name: "mix.m3u".to_owned(),
                    full_path: "mix.m3u".to_owned(),
                    last_modified: chrono::Utc::now(),
                    playlist: true,
                },
            ],
        );
        pane.populate_items();
        assert_eq!(pane.items.len(), 2, "playlists never show in the MPD browser");
        assert_eq!(pane.items[0].as_path(), "Downloads", "the downloads folder sits at the top");
        assert_eq!(pane.items[1].as_path(), "A");
    }

    /// Hidden directories (name starting with `.`, e.g. `.hist`) never
    /// appear in the MPD browser: the tree skips them (with their whole
    /// subtree) and the children list filters them out.
    #[test]
    fn hidden_directories_are_excluded() {
        let mut ctx = test_ctx();
        // The tree skips `.hist` and everything under it.
        let tree = DirTree::build(
            [
                "A".to_owned(),
                "A/B".to_owned(),
                ".hist".to_owned(),
                ".hist/old".to_owned(),
            ]
            .into_iter(),
        );
        let names: Vec<&str> = tree.root.children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["A"], "hidden dirs never become tree rows");
        assert_eq!(
            tree.root.children[0].children.len(),
            1,
            "visible subtrees keep their children"
        );

        // The children list hides hidden dirs too (files are kept).
        let mut pane = DirectoriesPane::new(&ctx);
        pane.selected = None;
        pane.loaded.insert(
            Path::new(),
            vec![dir("A", "A"), dir(".hist", ".hist"), song("track.flac")],
        );
        pane.populate_items();
        let shown: Vec<&str> = pane.items.iter().map(|i| i.as_path()).collect();
        assert_eq!(
            shown,
            ["Downloads", "A", "track.flac"],
            "hidden dirs never list, files stay (Downloads is injected)"
        );
    }

    /// Rapidly opening two folders supersedes the first `lsinfo` fetch
    /// (the client task skips it without firing its callback). The
    /// superseded path must not stay in `pending` forever — reopening it
    /// must issue a fresh fetch instead of showing an empty pane.
    #[test]
    fn superseded_fetch_never_sticks_in_pending() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        // Keep the app/work receivers alive; hold the client receiver so
        // the fetch requests are observable (and never fail to send).
        std::mem::forget(app_rx);
        std::mem::forget(work_rx);
        let _client_rx = client_rx;
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, crossbeam::channel::unbounded().1),
            (work_tx, crossbeam::channel::unbounded().1),
            (client_tx, crossbeam::channel::unbounded().1),
        );
        let mut pane = DirectoriesPane::new(&ctx);
        pane.tree = DirTree::build(["A".to_owned(), "B".to_owned()].into_iter());
        pane.loaded.insert(Path::new(), vec![dir("A", "A"), dir("B", "B")]);
        pane.selected = None;
        pane.populate_items();

        // Open A, back out, then open B before A's fetch resolves — B's
        // fetch supersedes A's in the client task, so A's callback never
        // fires. (Index 1 = A; index 0 is the injected Downloads row.)
        pane.item_list.select(Some(1));
        let mut ev = action(vec![Actions::Directories(DirectoriesActions::FolderExpand)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.pending.contains(&Path::from(["A"])), "A is being fetched");

        let mut ev = action(vec![Actions::Directories(DirectoriesActions::FolderCollapse)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.selected.is_none(), "back at the root");

        pane.item_list.select(Some(2));
        let mut ev = action(vec![Actions::Directories(DirectoriesActions::FolderExpand)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.pending.contains(&Path::from(["B"])), "B is being fetched");
        assert!(
            pane.pending.contains(&Path::from(["A"])),
            "A's superseded fetch is still unresolved"
        );

        // B's fetch resolves — the only one the client task would run.
        pane.on_query_finished(
            FETCH_DATA,
            MpdQueryResult::DirOrSong {
                data: vec![song("B/track.flac")],
                path: Some(Path::from(["B"])),
            },
            true,
            &ctx,
        )
        .unwrap();
        assert!(pane.pending.is_empty(), "superseded fetches never stick in pending");
        assert_eq!(pane.items.len(), 1, "B's children are shown");

        // Back out and reopen A — a fresh fetch is issued instead of
        // silently showing nothing.
        let mut ev = action(vec![Actions::Directories(DirectoriesActions::FolderCollapse)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.selected.is_none());
        assert!(pane.pending.is_empty());

        pane.item_list.select(Some(1));
        let mut ev = action(vec![Actions::Directories(DirectoriesActions::FolderExpand)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.pending.contains(&Path::from(["A"])), "a fresh fetch is issued for A");
    }
/// The MPD pane's right pane supports the queue tab's multi-selection:
/// ctrl+click toggles a mark, alt+click ranges from the anchor, plain
/// clicks clear the marks, Shift+Up/Down range-selects — and the rows
/// render directories with a ▶ prefix and songs with no D/S marker.
    use super::*;
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
    fn ctrl_alt_and_plain_clicks_mark_and_clear() {
        let ctx = test_ctx();
        let mut pane = pane_with_items(&ctx);
        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 30), &ctx).unwrap())
            .unwrap();
        let inner = pane.items_inner;

        // Plain click on row 2 sets the anchor; ctrl+click row 4 toggles.
        pane.handle_mouse_event(
            MouseEvent {
                x: inner.x + 1,
                y: inner.y + 2,
                kind: MouseEventKind::LeftClick,
                modifiers: KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        pane.handle_mouse_event(
            MouseEvent {
                x: inner.x + 1,
                y: inner.y + 4,
                kind: MouseEventKind::LeftClick,
                modifiers: KeyModifiers::CONTROL,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![4]);

        // alt+click row 3 ranges from the anchor (2) to 3.
        pane.handle_mouse_event(
            MouseEvent {
                x: inner.x + 1,
                y: inner.y + 3,
                kind: MouseEventKind::LeftClick,
                modifiers: KeyModifiers::ALT,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![2, 3, 4]);

        // A plain click on a different row clears the marks.
        pane.handle_mouse_event(
            MouseEvent {
                x: inner.x + 1,
                y: inner.y + 1,
                kind: MouseEventKind::LeftClick,
                modifiers: KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        assert!(pane.marked.is_empty());
        assert_eq!(pane.item_list.selected(), Some(1));
    }

    #[test]
    fn items_render_play_for_dirs_and_no_markers_for_songs() {
        let ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.items = vec![dir("Folder", "Folder"), song("alpha_song.flac")];

        let backend = ratatui::backend::TestBackend::new(60, 16);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 60, 16), &ctx).unwrap())
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..16u16)
            .map(|y| {
                (0..60u16).map(|x| buffer[(x, y)].symbol().to_string()).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("▶ Folder"), "directories get the ▶ prefix: {text}");
        assert!(!text.contains('D'), "no D marker on the folder row: {text}");
        assert!(!text.contains("S alpha"), "no S marker on the song row: {text}");
        assert!(text.contains("alpha_song"), "the song row still shows the file: {text}");
    }

    #[test]
    fn marked_rows_render_with_the_marked_style() {
        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        pane.items = vec![song("a.flac"), song("b.flac"), song("c.flac")];
        pane.item_list.select(Some(0));
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.marked.iter().collect::<Vec<_>>(), vec![0, 1]);

        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 30), &ctx).unwrap())
            .unwrap();
        let buffer = terminal.backend().buffer();
        let marked_style = ctx.config.theme.marked_item_style;
        let plain_style = ctx.config.as_list_name_style();
        // Sample inside the items list (right pane, past its border). The
        // marked rows render with the lighter marked highlight; the row the
        // cursor sits on (row 0) keeps the List's accent highlight, and a
        // plain row keeps the list style.
        // x=27 sits inside the items list (right pane, past its border).
        let row_bg = |y: u16| buffer[(27, y)].style().bg;
        // The marked rows render with the marked highlight; the row the
        // cursor sits on (row 1, the shift+down target) keeps the List's
        // accent highlight; the plain row keeps the list background.
        assert_eq!(row_bg(1), marked_style.bg, "row 0 is marked");
        assert_eq!(row_bg(2), ctx.config.theme.current_item_style.bg, "cursor row keeps the accent");
        assert!(
            !matches!(row_bg(3), Some(ratatui::style::Color::Rgb(92, 92, 92)) | Some(ratatui::style::Color::Rgb(71, 71, 71))),
            "row 2 keeps the plain list background"
        );
    }

    /// The tree pane is hidden entirely on TUIs ≤ 120 columns wide: the
    /// right pane gets the whole area (its inner rect starts at x=0), and
    /// the tree inner stays unset so mouse events over the right pane can
    /// never hit the tree.
    #[test]
    fn tree_pane_hidden_on_narrow_tui() {
        assert_eq!(tree_width(120), 0);
        assert_eq!(tree_width(80), 0);

        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 30), &ctx).unwrap())
            .unwrap();
        assert_eq!(pane.tree_inner, Rect::default(), "tree not rendered on a narrow TUI");
        // The right pane spans the whole width (x=1 is its border offset;
        // 78 = 80 minus the 2 border columns).
        assert_eq!(pane.items_inner.x, 1, "right pane starts at the left edge");
        assert_eq!(pane.items_inner.width, 78, "right pane takes the whole width");
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

        let mut ctx = test_ctx();
        let mut pane = DirectoriesPane::new(&ctx);
        let backend = ratatui::backend::TestBackend::new(160, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 160, 30), &ctx).unwrap())
            .unwrap();
        assert_eq!(
            pane.tree_inner.width,
            48,
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
            matches!(app_rx.try_recv(), Ok(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_)))),
            "Enter on a folder opens the context menu"
        );

        // Enter on the song opens the song's context menu.
        pane.item_list.select(Some(1));
        let mut ev = action(vec![Actions::Common(CommonAction::Confirm)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(
            matches!(app_rx.try_recv(), Ok(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_)))),
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
}

use super::*;

impl JellyfinPane {
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
    pub(super) fn rebuild_tree(&mut self) {
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
    pub(super) fn populate_items(&mut self) {
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
    pub(super) fn sync_tree_to_items_cursor(&mut self) {
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
    /// Open a highlighted container item: expand it in the tree and show
    /// its children in the right pane (used by Enter, `→` and double-click).
    pub(super) fn open_item(&mut self, item: JfItem, ctx: &Ctx) -> Result<()> {
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
    fn layout_vertical(&mut self, right: Rect) -> (Rect, Rect) {
        JellyfinPane::layout_vertical(self, right)
    }
    fn item_row(&self, idx: usize, hovered: bool, ctx: &Ctx) -> ListItem<'static> {
        JellyfinPane::item_row(self, idx, hovered, ctx)
    }
    fn render_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        JellyfinPane::render_info(self, frame, area, ctx)
    }
    fn open_context_menu(
        &mut self,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        JellyfinPane::open_context_menu(self, ctx, anchor)
    }
    fn temp_play_id(&self) -> Option<u32> {
        JellyfinPane::temp_play_id(self)
    }
    fn set_temp_play_id(&mut self, id: Option<u32>) {
        JellyfinPane::set_temp_play_id(self, id);
    }
}


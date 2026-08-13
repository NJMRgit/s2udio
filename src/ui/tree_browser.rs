use anyhow::Result;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, ListItem, ListState, Paragraph},
};

use crate::{
    MpdQueryResult,
    config::{
        keys::{CommonAction, DirectoriesActions},
        tabs::{PaneType, TreeBrowserArgs},
    },
    ctx::Ctx,
    mpd::{commands::State, mpd_client::MpdClient},
    shared::{
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{UiEvent, panes::Pane},
};

/// One visible row of the left tree, in render terms. Panes expose their
/// tree as these rows (the MPD browser flattens its `DirTree` on demand,
/// jellyfin maps its `Vec<JfNode>`, radio its `Vec<RegionRow>`); the
/// shared [`TreeBrowserCore::render_tree`] needs nothing else.
#[derive(Debug, Clone)]
pub struct TreeRowView {
    pub label: String,
    pub depth: u8,
    pub expandable: bool,
    pub expanded: bool,
    /// Always-open root row (the MPD browser's `Library ↴`): no arrow,
    /// never toggled.
    pub root: bool,
}

/// A generic two-pane "tree + items" browser core, extracted from the
/// three hand-rolled copies (`DirectoriesPane` / `JellyfinPane` /
/// `RadioPane`): the left pane is a collapsible tree, the right pane the
/// selected node's children. All three re-implemented the same mechanics —
/// tree expansion, `move_tree`/`move_items`, `select_parent`,
/// `highlight_tree_node`, `set_expanded`, `render_tree`/`render_items`/
/// `render_tips`, scrollbar/mouse routing, the common action arms and the
/// temp-play lifecycle. This trait is the single implementation; per-pane
/// differences (tree row content, item rows, info box, menus, data
/// fetching) are the hooks below, following the Phase-1 `SongListCore`
/// pattern.
///
/// Phase-2 consolidation target of docs/design/Rewrite/ui-reuse-rewrite.md.
#[allow(unused)]
pub(in crate::ui) trait TreeBrowserCore: Pane {
    /// The right-pane item type (used by the shared `selected_item`).
    type Item: Clone;

    // ── required: the tree ─────────────────────────────────────────────

    /// The flat visible tree rows (pre-order, collapsed subtrees skipped).
    fn tree_rows(&self) -> Vec<TreeRowView>;
    /// The highlighted row index (the pane's own tree cursor).
    fn tree_selected(&self) -> usize;
    fn tree_list(&self) -> &ListState;
    fn tree_list_mut(&mut self) -> &mut ListState;
    /// The clickable tree area (set by render, used by the mouse handler).
    fn tree_area(&self) -> Rect;
    fn set_tree_area(&mut self, area: Rect);
    /// Highlight a tree row and mirror it in the right pane (the pane
    /// maps the row index to its node and shows the node's children).
    fn highlight_tree_node(&mut self, idx: usize, ctx: &Ctx) -> Result<()>;
    /// Expand/collapse the tree row at `idx` (the pane maps the index to
    /// its node; root rows are never toggled).
    fn set_expanded_idx(&mut self, idx: usize, expanded: bool, ctx: &Ctx) -> Result<()>;

    // ── required: the items ────────────────────────────────────────────

    fn items_len(&self) -> usize;
    fn items_list(&self) -> &ListState;
    fn items_list_mut(&mut self) -> &mut ListState;
    /// The clickable items area (set by render, used by the mouse handler).
    fn items_area(&self) -> Rect;
    fn set_items_area(&mut self, area: Rect);
    fn item_at(&self, idx: usize) -> Option<Self::Item>;
    /// Render one item row (marks / playing / hover styling included).
    fn item_row(&self, idx: usize, hovered: bool, ctx: &Ctx) -> ListItem<'static>;
    /// Row height in terminal lines (1 for single-line rows, 2 for
    /// name+subline rows).
    fn item_row_height(&self) -> u16 {
        1
    }

    // ── required: behavior hooks ───────────────────────────────────────

    /// `a` / `←`: back out one level (parent's children on the right, the
    /// branch left collapses). Radio's focus-aware back-out implements
    /// this too.
    fn select_parent(&mut self, ctx: &Ctx) -> Result<()>;
    /// `d` / `→` / Enter / double-click: open the highlighted container or
    /// play the highlighted item.
    fn activate_selected(&mut self, ctx: &Ctx) -> Result<()>;
    /// Enter / right-click: the context menu of the highlighted item.
    fn open_context_menu(&mut self, ctx: &Ctx) -> Result<()>;
    /// The bottom info box (fully pane-specific: file preview / poster +
    /// metadata / station info).
    fn render_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx);

    // ── temp-play lifecycle (the queue id of a `d`/play-temp entry) ────

    fn temp_play_id(&self) -> Option<u32>;
    fn set_temp_play_id(&mut self, id: Option<u32>);

    // ── rendering hooks ────────────────────────────────────────────────

    fn tree_title(&self) -> &'static str;
    /// The arrow glyph of a tree row ("▼ "/"▶ " expanded/collapsed;
    /// non-expandable rows get no arrow).
    fn tree_arrow(&self, row: &TreeRowView) -> &'static str {
        if row.expandable {
            if row.expanded { "▼ " } else { "▶ " }
        } else {
            ""
        }
    }
    fn tree_highlight(&self, hover_idx: Option<usize>, ctx: &Ctx) -> Style {
        if hover_idx == self.tree_list().selected() {
            ctx.config.theme.hovered_item_style
        } else {
            ctx.config.theme.current_item_style
        }
    }
    fn items_title(&self) -> String;
    fn items_highlight(&self, hover_idx: Option<usize>, ctx: &Ctx) -> Style {
        if hover_idx == self.items_list().selected() {
            ctx.config.theme.hovered_item_style
        } else {
            ctx.config.theme.current_item_style
        }
    }
    /// Keybinding hints, one line each, for the tips strip.
    fn tips_lines(&self, ctx: &Ctx) -> Vec<Line<'static>>;
    /// The tips strip area (radio insets it by one column).
    fn tips_area(&self, area: Rect) -> Rect {
        area
    }
    /// The tree-browser layout args (tree min width / hide threshold;
    /// the info-box cap is read by the panes that use the capped formula).
    /// Default: today's constants (50 / 120 / Some(15)); the browser
    /// panes override this with their configured args.
    fn tree_args(&self) -> TreeBrowserArgs {
        TreeBrowserArgs::default()
    }
    /// Split the pane horizontally into (tree, right). The default hides
    /// the tree at/below `tree_hide_below` columns and keeps a
    /// `tree_min_width`-column tree otherwise (the MPD + Jellyfin
    /// browsers); radio always shows its 30% region tree.
    fn split_tree(&self, area: Rect) -> (Rect, Rect) {
        let w = self.tree_args().tree_width(area.width);
        if w == 0 {
            (Rect::default(), area)
        } else {
            let [tree, right] = Layout::horizontal([
                Constraint::Length(w),
                Constraint::Length(area.width - w),
            ])
            .areas(area);
            (tree, right)
        }
    }
    /// Split the right side into (items, tips, info).
    fn layout_vertical(&self, right: Rect) -> (Rect, Rect, Rect) {
        let [items, tips, info] = Layout::vertical([
            Constraint::Percentage(60),
            Constraint::Length(3),
            Constraint::Percentage(33),
        ])
        .areas(right);
        (items, tips, info)
    }

    // ── defaulted behavior hooks ───────────────────────────────────────

    /// Whether the wheel scrolls the viewport only (round 32: Queue,
    /// Playlists, MPD, Help, Radio) instead of moving the selection.
    /// Jellyfin is NOT in the round-32 pane list, so it keeps the
    /// wheel-moves-selection behavior.
    fn wheel_scrolls_viewport(&self) -> bool {
        true
    }

    /// Called after the items cursor moves (keyboard or click): keep the
    /// tree highlight on the cursor. Radio's tree never follows the
    /// station cursor.
    fn on_items_cursor_moved(&mut self, ctx: &Ctx) -> Result<()> {
        let before = self.tree_selected();
        self.sync_tree_to_items_cursor();
        // The items cursor's keyboard move moved the tree cursor: scroll
        // the tree back to it (the wheel only scrolls the tree viewport,
        // so keyboard moves restore the standard scrolloff behavior).
        // Radio's sync is a no-op, so an untouched tree stays put.
        if self.tree_selected() != before {
            self.scroll_tree_selection_into_view(ctx);
        }
        ctx.render()?;
        Ok(())
    }
    /// Keep the tree highlight on the right-pane cursor (default no-op;
    /// the MPD/Jellyfin browsers implement it).
    fn sync_tree_to_items_cursor(&mut self) {}
    /// Focus changed to the tree / items (jellyfin/radio track which list
    /// Enter acts on).
    fn on_tree_focus(&mut self) {}
    fn on_items_focus(&mut self) {}
    /// Reconnected: refetch on the next show.
    fn on_reconnected(&mut self, ctx: &Ctx) -> Result<()> {
        self.before_show(ctx)
    }
    /// The tree pane was hidden (narrow TUI); panes reset their tree rect.
    fn on_tree_hidden(&mut self) {}
    /// Right-click on a tree row (default: nothing; the MPD browser opens
    /// the folder menu).
    fn tree_context_menu(&mut self, _idx: usize, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }
    /// Double-click on a tree row: highlight + toggle expandable rows.
    /// Jellyfin additionally opens leaf containers (seasons).
    fn on_tree_double_click(&mut self, idx: usize, ctx: &Ctx) -> Result<()> {
        self.highlight_tree_node(idx, ctx)?;
        if let Some(row) = self.tree_rows().get(idx).cloned()
            && !row.root
            && row.expandable
        {
            self.set_expanded_idx(idx, !row.expanded, ctx)?;
        }
        Ok(())
    }
    /// Plain left-click on an items row (default: select + follow). The
    /// MPD browser overrides with its ctrl/alt marking + anchor logic.
    fn handle_items_left_click(&mut self, row: usize, _event: &MouseEvent, ctx: &Ctx) -> Result<()> {
        if row < self.items_len() {
            self.items_list_mut().select(Some(row));
            self.on_items_cursor_moved(ctx)?;
        }
        Ok(())
    }
    /// Enter (default: open/play; the MPD browser opens the context menu).
    fn on_confirm(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.activate_selected(ctx)
    }
    /// Shift+Up/Down range selection (default: unhandled; the MPD browser
    /// marks the range). Returns whether the action was handled.
    fn on_select_range(&mut self, _dir: i64, _ctx: &mut Ctx) -> Result<bool> {
        Ok(false)
    }
    /// Ctrl+A select-all (default: unhandled; the MPD browser marks every
    /// row of the right items pane). Returns whether the action was
    /// handled.
    fn on_select_all(&mut self, _ctx: &mut Ctx) -> Result<bool> {
        Ok(false)
    }
    /// Esc with a selection (default: unhandled; the MPD browser clears the
    /// marks and consumes the keypress).
    fn on_close(&mut self, _ctx: &Ctx) -> Result<bool> {
        Ok(false)
    }

    // ── shared default behavior ────────────────────────────────────────

    /// The item under the items cursor.
    fn selected_item(&self) -> Option<Self::Item> {
        let idx = self.items_list().selected()?;
        self.item_at(idx)
    }

    /// Move the tree highlight (clamped); the highlighted node's children
    /// fill the right pane.
    fn move_tree(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = self.tree_rows().len();
        if len == 0 {
            return Ok(());
        }
        let current = self.tree_selected().min(len - 1) as i64;
        let new_idx = (current + dir).clamp(0, len as i64 - 1) as usize;
        if new_idx != current as usize {
            self.highlight_tree_node(new_idx, ctx)?;
            self.scroll_tree_selection_into_view(ctx);
        }
        Ok(())
    }

    /// Round 32: the wheel scrolls the tree viewport only — the highlight
    /// stays put and may leave the visible area. The offset clamps at the
    /// tree's ends.
    fn scroll_tree_viewport(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = self.tree_rows().len();
        let viewport = self.tree_area().height as usize;
        crate::ui::widgets::virtualized_list::scroll_viewport(
            self.tree_list_mut(),
            dir,
            ctx.config.scroll_amount.max(1),
            len,
            viewport,
        );
        ctx.render()?;
        Ok(())
    }

    /// Move the items cursor (clamped); the tree follows.
    fn move_items(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = self.items_len();
        if len == 0 {
            return Ok(());
        }
        let current = self.items_list().selected().unwrap_or(0) as i64;
        let new_idx = (current + dir).clamp(0, len as i64 - 1) as usize;
        if new_idx != current as usize {
            self.items_list_mut().select(Some(new_idx));
            self.scroll_items_selection_into_view(ctx);
            self.on_items_cursor_moved(ctx)?;
        }
        Ok(())
    }

    /// Round 32: the wheel scrolls the items viewport only — the highlight
    /// stays put and may leave the visible area. The offset clamps at the
    /// list's ends (row height accounted for, e.g. radio's two-line rows).
    fn scroll_items_viewport(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = self.items_len();
        let viewport =
            (self.items_area().height as usize) / (self.item_row_height().max(1) as usize);
        crate::ui::widgets::virtualized_list::scroll_viewport(
            self.items_list_mut(),
            dir,
            ctx.config.scroll_amount.max(1),
            len,
            viewport,
        );
        ctx.render()?;
        Ok(())
    }

    /// Scroll the tree list so the tree cursor is visible again after a
    /// keyboard move (the wheel only scrolls the viewport, so keyboard
    /// moves restore the standard scrolloff behavior).
    fn scroll_tree_selection_into_view(&mut self, ctx: &Ctx) {
        let len = self.tree_rows().len();
        let viewport = self.tree_area().height as usize;
        if len > 0 {
            // The MPD pane keeps its cursor in `tree.selected` and only
            // mirrors it into the ListState at render time; mirror it now
            // so the scroll uses the current cursor (Jellyfin/Radio keep
            // their cursor in the ListState, so this is a no-op there).
            let sel = self.tree_selected().min(len - 1);
            if self.tree_list().selected() != Some(sel) {
                self.tree_list_mut().select(Some(sel));
            }
        }
        crate::ui::widgets::virtualized_list::scroll_selection_into_view(
            self.tree_list_mut(),
            len,
            viewport,
            ctx.config.scrolloff,
        );
    }

    /// Scroll the items list so the items cursor is visible again after a
    /// keyboard move (the wheel only scrolls the viewport, so keyboard
    /// moves restore the standard scrolloff behavior).
    fn scroll_items_selection_into_view(&mut self, ctx: &Ctx) {
        let len = self.items_len();
        let viewport =
            (self.items_area().height as usize) / (self.item_row_height().max(1) as usize);
        crate::ui::widgets::virtualized_list::scroll_selection_into_view(
            self.items_list_mut(),
            len,
            viewport,
            ctx.config.scrolloff,
        );
    }

    /// The full pane layout: tree + items + tips + info. The pane's
    /// `Pane::render` delegates here (the only difference between the
    /// three panes' old `render` bodies was the split ratios, now the
    /// `split_tree`/`layout_vertical` hooks).
    fn render_tree_browser(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        let (tree_area, right) = self.split_tree(area);
        let (items_area, tips_area, info_area) = self.layout_vertical(right);
        if tree_area.width > 0 {
            self.render_tree(frame, tree_area, ctx);
        } else {
            self.on_tree_hidden();
        }
        self.render_items(frame, items_area, ctx);
        self.render_tips(frame, tips_area, ctx);
        self.render_info(frame, info_area, ctx);
        Ok(())
    }

    /// The left tree: bordered list of the visible rows (indent + arrow +
    /// label), hover highlight, the cursor row's accent.
    fn render_tree(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let rows = self.tree_rows();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(self.tree_title());
        let inner = block.inner(area);
        self.set_tree_area(inner);

        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            inner,
            self.tree_list().offset(),
            rows.len(),
            1,
        );
        let items: Vec<ListItem> = rows
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let indent = "  ".repeat(usize::from(row.depth));
                let arrow = if row.root { "" } else { self.tree_arrow(row) };
                let mut item =
                    ListItem::new(Line::from(Span::raw(format!("{indent}{arrow}{}", row.label))));
                if hover_idx == Some(idx) {
                    item = item.style(ctx.config.theme.hovered_item_style);
                }
                item
            })
            .collect();

        let len = rows.len();
        if len > 0 {
            let sel = self.tree_selected().min(len - 1);
            if self.tree_list().selected() != Some(sel) {
                self.tree_list_mut().select(Some(sel));
            }
        }
        ratatui::widgets::StatefulWidget::render(
            crate::ui::widgets::virtualized_list::VirtualizedList::new(items)
                .highlight_style(self.tree_highlight(hover_idx, ctx))
                .style(ctx.config.as_list_name_style()),
            inner,
            frame.buffer_mut(),
            self.tree_list_mut(),
        );
        ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
    }

    /// The right items list: bordered list of the pane's rows, hover
    /// highlight, the cursor row's accent.
    fn render_items(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            // `items_title` is the title as it appears left of the count,
            // pre-padded per pane (" Library", " Items ", " Stations ") —
            // the shared format only appends "(n)" so each pane keeps its
            // own pre-Phase-2 spacing (Phase 2.1 parity close-out).
            .title(format!("{}({}) ", self.items_title(), self.items_len()));
        let inner = block.inner(area);
        self.set_items_area(inner);

        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            inner,
            self.items_list().offset(),
            self.items_len(),
            self.item_row_height(),
        );
        let items: Vec<ListItem> = (0..self.items_len())
            .map(|idx| self.item_row(idx, hover_idx == Some(idx), ctx))
            .collect();

        ratatui::widgets::StatefulWidget::render(
            crate::ui::widgets::virtualized_list::VirtualizedList::new(items)
                .highlight_style(self.items_highlight(hover_idx, ctx))
                .style(ctx.config.as_list_name_style())
                .row_height(self.item_row_height()),
            inner,
            frame.buffer_mut(),
            self.items_list_mut(),
        );
        ratatui::widgets::Widget::render(block, area, frame.buffer_mut());
    }

    /// The keybinding hints strip between the items list and the info box.
    fn render_tips(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let dim = ctx.config.as_list_text_style();
        frame.render_widget(
            Paragraph::new(self.tips_lines(ctx)).style(dim),
            self.tips_area(area),
        );
    }

    /// Drop the temporary play entry once playback has moved on.
    fn cleanup_temp_play(&mut self, ctx: &Ctx) {
        if let Some(temp) = self.temp_play_id()
            && ctx.status.songid != Some(temp)
        {
            self.set_temp_play_id(None);
            ctx.temp_play_id.set(None);
            ctx.command(move |client| {
                client.delete_id(temp)?;
                Ok(())
            });
        }
    }

    /// Drop the temporary entry on the stop transition itself (streams
    /// keep the same queue entry while playing, so SongChanged never fires
    /// for them; after stop MPD still reports the last songid).
    fn temp_play_on_stop(&mut self, ctx: &Ctx) {
        if ctx.status.state == State::Stop
            && let Some(temp) = self.temp_play_id()
        {
            self.set_temp_play_id(None);
            ctx.temp_play_id.set(None);
            ctx.command(move |client| {
                client.delete_id(temp)?;
                Ok(())
            });
        }
    }

    /// Drop any previous temporary entry (repeatedly playing files must
    /// never grow the queue: the SongChanged cleanup below only fires when
    /// the song actually moves on, which can lag or miss consecutive
    /// plays).
    fn drop_temp_play(&mut self, ctx: &Ctx) {
        if let Some(prev) = self.temp_play_id() {
            self.set_temp_play_id(None);
            ctx.temp_play_id.set(None);
            ctx.command(move |client| {
                client.delete_id(prev)?;
                Ok(())
            });
        }
    }

    /// Play a stream URL as a temporary (queue-free) MPD entry: drop any
    /// previous one, then `addid` + `playid`. The result arrives via
    /// [`TreeBrowserCore::handle_play_result`] and records the id.
    fn play_temp_url(&mut self, ctx: &Ctx, id: &'static str, pane: PaneType, url: String) {
        self.drop_temp_play(ctx);
        ctx.query().id(id).replace_id(id).target(pane).query(move |client| {
            let id = client.add_id(&url, None)?;
            client.play_id(id)?;
            Ok(MpdQueryResult::Any(Box::new(id)))
        });
    }

    /// Record the queue id of a temp-play query result (shared by the
    /// panes' `on_query_finished` play arms).
    fn handle_play_result(&mut self, any: Box<dyn std::any::Any + Send + Sync>, ctx: &Ctx) -> Result<()> {
        if let Ok(boxed) = any.downcast::<u32>() {
            self.set_temp_play_id(Some(*boxed));
            ctx.temp_play_id.set(Some(*boxed));
        }
        Ok(())
    }

    /// The tree/items temp-play event arms (`SongChanged`,
    /// `PlaybackStateChanged`, `Player`, `Reconnected`) — the shared part
    /// of the three panes' `on_event` bodies. Returns whether the event
    /// was handled.
    fn handle_tree_events(&mut self, event: &mut UiEvent, _is_visible: bool, ctx: &Ctx) -> Result<bool> {
        match event {
            UiEvent::SongChanged => self.cleanup_temp_play(ctx),
            UiEvent::PlaybackStateChanged | UiEvent::Player => self.temp_play_on_stop(ctx),
            UiEvent::Reconnected => self.on_reconnected(ctx)?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Mouse routing for the tree + items areas (the MPD browser's whole
    /// `handle_mouse_event`; jellyfin/radio insert their extra arms around
    /// it).
    fn handle_tree_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if self.tree_area().contains(event.into()) {
            return self.handle_tree_mouse(event, ctx);
        }
        if self.items_area().contains(event.into()) {
            return self.handle_items_mouse(event, ctx);
        }
        Ok(())
    }

    /// Tree-area mouse: click highlights, double-click toggles (or opens),
    /// right-click opens the pane's tree menu, wheel moves the highlight.
    fn handle_tree_mouse(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        self.on_tree_focus();
        let row = usize::from(event.y.saturating_sub(self.tree_area().y)) + self.tree_list().offset();
        match event.kind {
            MouseEventKind::LeftClick => self.highlight_tree_node(row, ctx),
            MouseEventKind::DoubleClick => self.on_tree_double_click(row, ctx),
            MouseEventKind::RightClick => self.tree_context_menu(row, ctx),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                if self.wheel_scrolls_viewport() {
                    self.scroll_tree_viewport(dir, ctx)
                } else {
                    self.move_tree(dir, ctx)
                }
            }
            _ => Ok(()),
        }
    }

    /// Items-area mouse: left click selects (+ marks in the MPD browser),
    /// double-click opens/plays, right-click opens the context menu, wheel
    /// moves the cursor.
    fn handle_items_mouse(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        self.on_items_focus();
        let row = usize::from(event.y.saturating_sub(self.items_area().y))
            / usize::from(self.item_row_height())
            + self.items_list().offset();
        match event.kind {
            MouseEventKind::LeftClick => {
                self.handle_items_left_click(row, &event, ctx)?;
            }
            MouseEventKind::DoubleClick => {
                if row < self.items_len() {
                    self.items_list_mut().select(Some(row));
                    self.on_items_cursor_moved(ctx)?;
                    self.activate_selected(ctx)?;
                }
            }
            MouseEventKind::RightClick => {
                if row < self.items_len() {
                    self.items_list_mut().select(Some(row));
                    self.on_items_cursor_moved(ctx)?;
                    self.open_context_menu(ctx)?;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                if self.wheel_scrolls_viewport() {
                    self.scroll_items_viewport(dir, ctx)?;
                } else {
                    self.move_items(dir, ctx)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The common + directories action arms shared by the MPD and Jellyfin
    /// browsers: `w/s`/arrows move the right-pane list, `a`/`←` back out,
    /// `d`/`→` open/play, Enter/right-click open the context menu. Returns
    /// whether an action was claimed.
    fn handle_tree_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<bool> {
        if let Some(action) = event.claim_common() {
            match action {
                CommonAction::Up | CommonAction::Down => {
                    self.on_items_focus();
                    let dir = if matches!(action, CommonAction::Up) { -1 } else { 1 };
                    self.move_items(dir, ctx)?;
                }
                CommonAction::Left => {
                    self.on_items_focus();
                    self.select_parent(ctx)?;
                }
                CommonAction::Top => {
                    if self.items_len() > 0 {
                        self.items_list_mut().select(Some(0));
                        self.on_items_cursor_moved(ctx)?;
                    }
                }
                CommonAction::Bottom => {
                    let len = self.items_len();
                    if len > 0 {
                        self.items_list_mut().select(Some(len - 1));
                        self.on_items_cursor_moved(ctx)?;
                    }
                }
                CommonAction::SelectUp | CommonAction::SelectDown => {
                    let dir = if matches!(action, CommonAction::SelectDown) { 1 } else { -1 };
                    if !self.on_select_range(dir, ctx)? {
                        event.abandon();
                    }
                }
                CommonAction::SelectAll => {
                    // Ctrl+A marks every row of the right (items) pane —
                    // the multi-selectable list of the MPD tab (the folder
                    // tree on the left is not part of the selection).
                    if !self.on_select_all(ctx)? {
                        event.abandon();
                    }
                }
                CommonAction::Confirm => self.on_confirm(ctx)?,
                CommonAction::ContextMenu => self.open_context_menu(ctx)?,
                CommonAction::Close => {
                    if self.on_close(ctx)? {
                        event.consume();
                    } else {
                        event.abandon();
                    }
                }
                _ => event.abandon(),
            }
            return Ok(true);
        }
        if let Some(action) = event.claim_directories() {
            match action {
                DirectoriesActions::FolderUp | DirectoriesActions::FolderDown => {
                    self.on_tree_focus();
                    let dir = if matches!(action, DirectoriesActions::FolderUp) { -1 } else { 1 };
                    self.move_tree(dir, ctx)?;
                }
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    self.activate_selected(ctx)?;
                }
                DirectoriesActions::FolderCollapse => {
                    self.on_items_focus();
                    self.select_parent(ctx)?;
                }
            }
            return Ok(true);
        }
        Ok(false)
    }
}

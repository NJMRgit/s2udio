use anyhow::{Context, Result};
use ratatui::{Frame, buffer::Buffer, layout::Position, prelude::Rect};
use super::Pane;
use crate::{
    config::tabs::{QUEUE_TAB_NAME, TabName}, ctx::Ctx,
    shared::{
        events::AppEvent, keys::ActionEvent, macros::modal,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{
        UiAppEvent, UiEvent, modals::settings::SettingsModal,
        modals::tab_help::TabHelpModal,
    },
};
/// The two-group tab bar. Round 60c (S2) draws it as ONE real box per the
/// design mock — the pane renders its own top/bottom borders (its config
/// border is suppressed in `src/ui/panes/mod.rs`), row 1 carries the five
/// cells `Queue │ Libraries │ (center) │ Help │ Settings` with `┬`
/// junction corners on the top border, the divider row is a CONNECTED
/// `├──┴──…──┴──┤` (the `┴` merges under the row-1 cells), and while a
/// library tab is active row 3 lists the libraries CENTERED (`MPD  •
/// Playlists  •  Downloads  •  Jellyfin  •  Radio` — Queue never appears
/// there). While the Queue tab is active the bar stays compact (top
/// border + row 1 + bottom border). The pane's height is dynamic
/// (3 rows compact / 5 rows expanded, `src/ui/panes/mod.rs`).
const SEP_PIPE: &str = "  │  ";
const SEP_BULLET: &str = "  •  ";
/// The Help / Settings buttons (the bar's right cells).
const HELP_LABEL: &str = "Help";
const SETTINGS_LABEL: &str = "Settings";
#[derive(Debug)]
struct BarItem {
    label: String,
    /// `Some(tab name)` for a real tab; `None` for the `Libraries` group
    /// pseudo-item (clicking it switches to the libraries group).
    tab: Option<TabName>,
}
#[derive(Debug)]
pub struct TabsPane {
    area: Rect,
    active_tab: TabName,
    /// Row-1 items: `Queue` (when configured) + the `Libraries` group.
    items: Vec<BarItem>,
    /// Row-2 items: the library tabs in canonical order.
    lib_items: Vec<BarItem>,
    /// Click areas of every item (row 1 then row 2), parallel to
    /// `item_tabs` (`None` = the Libraries group).
    areas: Vec<Rect>,
    item_tabs: Vec<Option<TabName>>,
    help_area: Rect,
    settings_area: Rect,
}
impl TabsPane {
    pub fn new(ctx: &Ctx) -> Result<Self> {
        let active_tab = ctx.active_tab.clone();
        Ok(Self {
            area: Rect::default(),
            active_tab,
            items: Self::build_items(ctx),
            lib_items: Self::build_lib_items(ctx),
            areas: Vec::new(),
            item_tabs: Vec::new(),
            help_area: Rect::default(),
            settings_area: Rect::default(),
        })
    }
    /// Row-1: the Queue tab (when present) then the Libraries group.
    fn build_items(ctx: &Ctx) -> Vec<BarItem> {
        let mut items = Vec::new();
        if let Some(queue) = ctx.config.tabs.names.iter().find(|name| {
            name.as_str().eq_ignore_ascii_case(QUEUE_TAB_NAME)
                && !ctx.config.is_tab_hidden(name)
        }) {
            items.push(BarItem {
                label: QUEUE_TAB_NAME.to_owned(),
                tab: Some(queue.clone()),
            });
        }
        items.push(BarItem {
            label: "Libraries".to_owned(),
            tab: None,
        });
        items
    }
    /// Row-2: the visible library tabs in the canonical order
    /// (MPD • Playlists • Downloads • Jellyfin • Radio), with any extra
    /// configured tabs appended so custom names stay reachable.
    fn build_lib_items(ctx: &Ctx) -> Vec<BarItem> {
        ctx.config
            .library_tabs_ordered()
            .into_iter()
            .map(|name| BarItem {
                label: name.to_string(),
                tab: Some(name),
            })
            .collect()
    }
    fn get_tab_idx_at(&self, position: Position) -> Option<usize> {
        self.areas
            .iter()
            .enumerate()
            .find(|(_, area)| area.contains(position))
            .map(|v| v.0)
    }
    fn open_help(&self, ctx: &Ctx) -> Result<()> {
        modal!(ctx, TabHelpModal::new(ctx));
        Ok(())
    }
    fn open_settings(&self, ctx: &Ctx) -> Result<()> {
        modal!(ctx, SettingsModal::new(ctx));
        Ok(())
    }
    /// Draw a piece of the bar at `x`, skipping anything that would reach or
    /// pass `right`; returns the next x position.
    fn draw_at(
        &self,
        buf: &mut Buffer,
        text: &str,
        x: u16,
        top: u16,
        right: u16,
        style: ratatui::style::Style,
    ) -> u16 {
        for (offset, ch) in text.char_indices() {
            let col = x + offset as u16;
            if col >= right {
                break;
            }
            buf[(col, top)].set_symbol(&ch.to_string()).set_style(style);
        }
        x + text.chars().count() as u16
    }
}
impl Pane for TabsPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> anyhow::Result<()> {
        self.area = area;
        if self.area.height == 0 {
            return Ok(());
        }
        let inactive = ctx.config.theme.tab_bar.inactive_style;
        let active = ctx.config.theme.tab_bar.active_style;
        let mouse = ctx.mouse_pos();
        let hovered = |style: ratatui::style::Style| crate::config::hover_style(style);
        self.areas = Vec::new();
        self.item_tabs = Vec::new();
        self.help_area = Rect::default();
        self.settings_area = Rect::default();
        let left = area.left();
        let right = area.right().saturating_sub(1);
        let top = area.top();
        let bottom = area.bottom().saturating_sub(1);
        // Round 61 (N2): the bar's box glyphs and the `│` cell separators
        // use the theme's border ACCENT (highlight_border_style), so a
        // real accent color shows through on any theme. The plain border
        // color keeps driving the config borders elsewhere.
        let accent = ctx.config.as_focused_border_style();
        let buf = frame.buffer_mut();
        // Round 60c (S2): the bar is a real box per the mock — the pane
        // draws its own top/bottom borders, the `│` cell separators and
        // the `├──┴──…──┴──┤` divider itself (its config border is
        // suppressed in `src/ui/panes/mod.rs`). Compact (Queue active):
        // top border + row 1 (`Queue │ Libraries │ (center) │ Help │
        // Settings`) + bottom border. Expanded (library active): top
        // border + row 1 + `├─┴─…─┤` divider + the CENTERED libraries
        // row + bottom border.
        let expanded = ctx.config.is_library_tab(&self.active_tab);
        let content_top = top + 1;
        // ── row-1 cell geometry ──────────────────────────────────────
        let queue_label = self.items.first().map(|i| i.label.as_str()).unwrap_or("");
        let libs_label = self.items.get(1).map(|i| i.label.as_str()).unwrap_or("Libraries");
        const CELL_PAD: u16 = 2;
        const SEP: u16 = 1;
        let cell_w = |label: &str| -> u16 { label.chars().count() as u16 + 2 * CELL_PAD };
        let queue_cw = cell_w(queue_label);
        let libs_cw = cell_w(libs_label);
        let help_cw = cell_w(HELP_LABEL.trim());
        let settings_cw = cell_w(SETTINGS_LABEL.trim());
        let inner_w = right.saturating_sub(left + 1);
        let base_cells = queue_cw
            .saturating_add(libs_cw)
            .saturating_add(help_cw)
            .saturating_add(settings_cw)
            .saturating_add(4 * SEP);
        let center_cw = inner_w.saturating_sub(base_cells);
        // Cell x positions + the separator columns between them.
        let sep1 = left + 1 + queue_cw;
        let sep2 = sep1 + SEP + libs_cw;
        let sep3 = sep2 + SEP + center_cw;
        let sep4 = sep3 + SEP + help_cw;
        let cells = [
            (queue_cw, queue_label),
            (libs_cw, libs_label),
            (center_cw, ""),
            (help_cw, HELP_LABEL.trim()),
            (settings_cw, SETTINGS_LABEL.trim()),
        ];
        let seps = [sep1, sep2, sep3, sep4];
        // ── the box ──────────────────────────────────────────────────
        if area.height >= 3 {
            // Top border: `╭──┬──┬──┬──┬──╮` (junction corners at the
            // cell boundaries).
            buf[(left, top)].set_symbol("╭").set_style(accent);
            buf[(right, top)].set_symbol("╮").set_style(accent);
            for col in (left + 1)..right {
                buf[(col, top)].set_symbol("─").set_style(accent);
            }
            for sx in seps {
                if sx > left && sx < right {
                    buf[(sx, top)].set_symbol("┬").set_style(accent);
                }
            }
            // Row 1: the five cells with `│` separators (the separators
            // are part of the frame, so they take the border accent —
            // round 61 N2).
            let mut x = left + 1;
            for (idx, &(cw, label)) in cells.iter().enumerate() {
                if x < right {
                    buf[(x.saturating_sub(1), content_top)].set_symbol("│").set_style(accent);
                }
                if cw >= 2 && !label.is_empty() {
                    let label_w = label.chars().count() as u16;
                    let lx = x + (cw - label_w) / 2;
                    // Round 62 (N2): the row-1 `Libraries` group cell shows
                    // the same select effect `Queue` gets — while ANY
                    // library tab is active (the expanded bar). The
                    // pseudo-item has `tab: None`, so a plain tab equality
                    // never activated it.
                    let is_active = self.items.get(idx).is_some_and(|i| match &i.tab {
                        Some(tab) => *tab == self.active_tab,
                        None => expanded,
                    });
                    let cell_rect = Rect { x, y: content_top, width: cw, height: 1 };
                    let style = if is_active {
                        active
                    } else if mouse.is_some_and(|p| cell_rect.contains(p)) {
                        hovered(inactive)
                    } else {
                        inactive
                    };
                    self.draw_at(buf, label, lx, content_top, right, style);
                    self.areas.push(cell_rect);
                    self.item_tabs.push(self.items.get(idx).and_then(|i| i.tab.clone()));
                    if idx == 3 {
                        self.help_area = cell_rect;
                    }
                    if idx == 4 {
                        self.settings_area = cell_rect;
                    }
                }
                x += cw + SEP;
            }
            buf[(right, content_top)].set_symbol("│").set_style(accent);
            if expanded && area.height >= 5 {
                // The `├──┴──…──┴──┤` divider (the row-1 cells merge
                // into the single libraries row below).
                let divider_y = top + 2;
                buf[(left, divider_y)].set_symbol("├").set_style(accent);
                buf[(right, divider_y)].set_symbol("┤").set_style(accent);
                for col in (left + 1)..right {
                    buf[(col, divider_y)].set_symbol("─").set_style(accent);
                }
                for sx in seps {
                    if sx > left && sx < right {
                        buf[(sx, divider_y)].set_symbol("┴").set_style(accent);
                    }
                }
                // Row 3: the libraries row, CENTERED; both `│` ends are
                // painted (round 61 N3 — the right one used to be
                // missing, the row stopped at the text end).
                let lib_top = top + 3;
                buf[(left, lib_top)].set_symbol("│").set_style(accent);
                buf[(right, lib_top)].set_symbol("│").set_style(accent);
                let mut items_w = 0u16;
                for (idx, item) in self.lib_items.iter().enumerate() {
                    if idx > 0 {
                        items_w += SEP_BULLET.chars().count() as u16;
                    }
                    items_w += item.label.chars().count() as u16;
                }
                let row_w = right.saturating_sub(left + 1);
                let libs_x = if items_w < row_w {
                    left + 1 + (row_w - items_w) / 2
                } else {
                    left + 1
                };
                let mut x = libs_x;
                for (idx, item) in self.lib_items.iter().enumerate() {
                    if idx > 0 {
                        x = self.draw_at(buf, SEP_BULLET, x, lib_top, right, inactive);
                    }
                    if x >= right {
                        break;
                    }
                    let is_active = item.tab.as_ref().is_some_and(|tab| *tab == self.active_tab);
                    let label_width = item.label.chars().count() as u16;
                    let item_area = Rect {
                        x,
                        y: lib_top,
                        width: label_width.min(right.saturating_sub(x)),
                        height: 1,
                    };
                    let style = if is_active {
                        active
                    } else if mouse.is_some_and(|p| item_area.contains(p)) {
                        hovered(inactive)
                    } else {
                        inactive
                    };
                    self.draw_at(buf, &item.label, x, lib_top, right, style);
                    self.areas.push(item_area);
                    self.item_tabs.push(item.tab.clone());
                    x += label_width;
                }
            }
            // Bottom border (round 61 N1 / round 62 N3): the COMPACT bar
            // (Queue active) keeps the `┴` junction corners mirroring the
            // top `┬` grid at the same cell boundaries; the EXPANDED bar
            // bottom is a plain continuous `╰─────╯` line per the user's
            // fixed diagram (the top `┬` grid and the `├─┴─…─┤` divider
            // stay).
            buf[(left, bottom)].set_symbol("╰").set_style(accent);
            buf[(right, bottom)].set_symbol("╯").set_style(accent);
            for col in (left + 1)..right {
                buf[(col, bottom)].set_symbol("─").set_style(accent);
            }
            if !expanded {
                for sx in seps {
                    if sx > left && sx < right {
                        buf[(sx, bottom)].set_symbol("┴").set_style(accent);
                    }
                }
            }
        } else {
            // Degenerate height: label row only, no box.
            let mut x = left + 1;
            for (idx, item) in self.items.iter().enumerate() {
                if idx > 0 {
                    x = self.draw_at(buf, SEP_PIPE, x, top, right, inactive);
                }
                let item_area = Rect {
                    x,
                    y: top,
                    width: (item.label.chars().count() as u16).min(right.saturating_sub(x)),
                    height: 1,
                };
                let style = if item.tab.as_ref().is_some_and(|tab| *tab == self.active_tab)
                    || (item.tab.is_none() && expanded)
                {
                    active
                } else {
                    inactive
                };
                self.draw_at(buf, &item.label, x, top, right, style);
                self.areas.push(item_area);
                self.item_tabs.push(item.tab.clone());
                x += item.label.chars().count() as u16;
            }
        }
        Ok(())
    }
    fn before_show(&mut self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &mut UiEvent, _is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::TabChanged(tab) => {
                self.active_tab = tab.clone();
                ctx.render()?;
            }
            UiEvent::ConfigChanged => {
                let new_active_tab = ctx
                    .config
                    .tabs
                    .names
                    .iter()
                    .find(|tab| tab == &&self.active_tab)
                    .or(ctx.config.tabs.names.first())
                    .context("Expected at least one tab")
                    .cloned()?;
                self.items = Self::build_items(ctx);
                self.lib_items = Self::build_lib_items(ctx);
                self.active_tab = new_active_tab;
            }
            _ => {}
        }
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if !self.area.contains(event.into()) {
            return Ok(());
        }
        if !matches!(
            event.kind, MouseEventKind::LeftClick | MouseEventKind::DoubleClick
        ) {
            return Ok(());
        }
        if self.help_area.contains(event.into()) {
            return self.open_help(ctx);
        }
        if self.settings_area.contains(event.into()) {
            return self.open_settings(ctx);
        }
        let Some(idx) = self.get_tab_idx_at(event.into()) else {
            return Ok(());
        };
        let Some(tab) = self.item_tabs.get(idx).cloned().flatten() else {
            // The Libraries group: switch to the libraries (the last
            // browsed library tab, or the first canonical one).
            return ctx
                .app_event_sender
                .send(AppEvent::UiEvent(UiAppEvent::SwitchToLibraries))
                .map_err(Into::into);
        };
        if self.active_tab != tab {
            ctx.app_event_sender
                .send(AppEvent::UiEvent(UiAppEvent::ChangeTab(tab.clone())))?;
        }
        Ok(())
    }
    fn handle_action(&mut self, _event: &mut ActionEvent, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

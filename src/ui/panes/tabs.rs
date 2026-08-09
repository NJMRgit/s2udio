use anyhow::{Context, Result};
use ratatui::{Frame, layout::Position, prelude::Rect};

use super::Pane;
use crate::{
    config::tabs::TabName,
    ctx::Ctx,
    shared::{
        events::AppEvent,
        keys::ActionEvent,
        macros::modal,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{UiAppEvent, UiEvent, modals::tab_help::TabHelpModal, modals::settings::SettingsModal},
};

/// The tab bar. Tabs sit on the left, grouped with `│` separators and `•`
/// bullets between the groups; `Help | Settings` sits right-aligned at the
/// end of the bar. When the terminal is narrow the separators shrink from
/// double to single spaces so both groups still fit.
const BULLET_AFTER: [&str; 3] = ["MPD", "Jellyfin", "Radio"];
const SEP_PIPE: &str = "  │  ";
const SEP_BULLET: &str = "  •  ";
const SEP_PIPE_TIGHT: &str = " │ ";
const SEP_BULLET_TIGHT: &str = " • ";

/// Right-aligned buttons rendered at the end of the bar.
const HELP_LABEL: &str = " Help ";
const SETTINGS_LABEL: &str = " Settings ";
const BUTTON_SEP: &str = "|";

#[derive(Debug)]
struct BarItem {
    label: String,
    /// `Some(tab name)` for a real tab.
    tab: Option<TabName>,
}

#[derive(Debug)]
pub struct TabsPane {
    area: Rect,
    active_tab: TabName,
    items: Vec<BarItem>,
    areas: Vec<Rect>,
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
            areas: Vec::new(),
            help_area: Rect::default(),
            settings_area: Rect::default(),
        })
    }

    fn build_items(ctx: &Ctx) -> Vec<BarItem> {
        ctx.config
            .tabs
            .names
            .iter()
            .filter(|name| !ctx.config.is_tab_hidden(name))
            .map(|name| BarItem { label: name.to_string(), tab: Some(name.clone()) })
            .collect()
    }

    fn get_tab_idx_at(&self, position: Position) -> Option<usize> {
        self.areas.iter().enumerate().find(|(_, area)| area.contains(position)).map(|v| v.0)
    }

    fn open_help(&self, ctx: &Ctx) -> Result<()> {
        modal!(ctx, TabHelpModal::new(ctx));
        Ok(())
    }

    fn open_settings(&self, ctx: &Ctx) -> Result<()> {
        modal!(ctx, SettingsModal::new(ctx));
        Ok(())
    }

    /// Whether the tab group needs the single-space separators to fit next to
    /// the right-aligned buttons.
    fn uses_tight_separators(&self, area: Rect, right_x: u16) -> bool {
        let loose: usize = self
            .items
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                item.label.chars().count()
                    + if idx == 0 {
                        0
                    } else if BULLET_AFTER.contains(
                        &self.items[idx - 1]
                            .tab
                            .as_ref()
                            .expect("tab items have a name")
                            .as_str(),
                    ) {
                        SEP_BULLET.chars().count()
                    } else {
                        SEP_PIPE.chars().count()
                    }
            })
            .sum();
        let tight: usize = self
            .items
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                item.label.chars().count()
                    + if idx == 0 {
                        0
                    } else if BULLET_AFTER.contains(
                        &self.items[idx - 1]
                            .tab
                            .as_ref()
                            .expect("tab items have a name")
                            .as_str(),
                    ) {
                        SEP_BULLET_TIGHT.chars().count()
                    } else {
                        SEP_PIPE_TIGHT.chars().count()
                    }
            })
            .sum();
        let start_offset = 1usize;
        let available = right_x.saturating_sub(area.left()) as usize;
        // Prefer the double-space look; fall back when it would overlap.
        loose + start_offset > available && tight + start_offset <= available
    }

    /// Draw a piece of the bar at `x`, skipping anything that would reach or
    /// pass `right`; returns the next x position.
    fn draw_at(
        &self,
        frame: &mut Frame,
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
            frame.buffer_mut()[(col, top)].set_symbol(&ch.to_string()).set_style(style);
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
        // Hovering a clickable label lightens it (lighter, less saturated).
        let mouse = ctx.mouse_pos();
        let hovered = |style: ratatui::style::Style| crate::config::hover_style(style);

        self.areas = Vec::new();
        self.help_area = Rect::default();
        self.settings_area = Rect::default();

        let top = area.top();
        let right = area.right();

        // Right group: Help | Settings, right-aligned.
        let right_group_width = (HELP_LABEL.chars().count()
            + BUTTON_SEP.chars().count()
            + SETTINGS_LABEL.chars().count()) as u16;
        let right_x = right.saturating_sub(right_group_width);
        // Left group is clipped before the right group so the two never
        // overlap; the tight-fit check below keeps a safe margin.
        let tabs_right_bound = right_x;

        // Choose separator spacing so the tab group fits next to the buttons.
        let tight = self.uses_tight_separators(area, right_x);

        // Left group: the tabs, starting one cell in from the border.
        let mut x = area.left().saturating_add(1);
        for (idx, item) in self.items.iter().enumerate() {
            let sep = if idx == 0 {
                ""
            } else if BULLET_AFTER.contains(
                &self.items[idx - 1].tab.as_ref().expect("tab items have a name").as_str(),
            ) {
                if tight { SEP_BULLET_TIGHT } else { SEP_BULLET }
            } else if tight {
                SEP_PIPE_TIGHT
            } else {
                SEP_PIPE
            };
            if !sep.is_empty() {
                x = self.draw_at(frame, sep, x, top, tabs_right_bound, inactive);
            }
            let label_width = item.label.chars().count() as u16;
            let is_active =
                item.tab.as_ref().is_some_and(|tab| *tab == self.active_tab);
            let item_area = Rect { x, y: top, width: label_width, height: 1 };
            // Only clickable labels hover (the active tab is not).
            let style = if is_active {
                active
            } else if mouse.is_some_and(|p| item_area.contains(p)) {
                hovered(inactive)
            } else {
                inactive
            };
            self.draw_at(frame, &item.label, x, top, tabs_right_bound, style);
            self.areas.push(item_area);
            x += label_width;
        }

        if right_x >= x {
            self.help_area = Rect {
                x: right_x,
                y: top,
                width: HELP_LABEL.chars().count() as u16,
                height: 1,
            };
            let help_style = if mouse.is_some_and(|p| self.help_area.contains(p)) {
                hovered(inactive)
            } else {
                inactive
            };
            self.draw_at(frame, HELP_LABEL, right_x, top, right, help_style);
            // The separator between the two buttons.
            self.draw_at(
                frame,
                BUTTON_SEP,
                right_x + HELP_LABEL.chars().count() as u16,
                top,
                right,
                inactive,
            );
            // Settings starts right after Help and the separator.
            let settings_x = right_x
                + HELP_LABEL.chars().count() as u16
                + BUTTON_SEP.chars().count() as u16;
            self.settings_area = Rect {
                x: settings_x,
                y: top,
                width: SETTINGS_LABEL.chars().count() as u16,
                height: 1,
            };
            let settings_style = if mouse.is_some_and(|p| self.settings_area.contains(p)) {
                hovered(inactive)
            } else {
                inactive
            };
            self.draw_at(frame, SETTINGS_LABEL, settings_x, top, right, settings_style);
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
        if !matches!(event.kind, MouseEventKind::LeftClick | MouseEventKind::DoubleClick) {
            return Ok(());
        }

        // Help / Settings buttons?
        if self.help_area.contains(event.into()) {
            return self.open_help(ctx);
        }
        if self.settings_area.contains(event.into()) {
            return self.open_settings(ctx);
        }

        let Some(tab_name) = self
            .get_tab_idx_at(event.into())
            .and_then(|idx| self.items.get(idx).and_then(|i| i.tab.clone()))
        else {
            return Ok(());
        };

        if self.active_tab != tab_name {
            ctx.app_event_sender.send(AppEvent::UiEvent(UiAppEvent::ChangeTab(tab_name.clone())))?;
        }

        Ok(())
    }

    fn handle_action(&mut self, _event: &mut ActionEvent, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Rect,
    };

    use super::*;
    use crate::tests::fixtures::ctx;

    /// Render the bar at the given terminal width and return the text row.
    fn bar_line(ctx: &Ctx, width: u16) -> String {
        let backend = TestBackend::new(width, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut pane = TabsPane::new(ctx).unwrap();
        terminal
            .draw(|frame| {
                let inner = Rect { x: 1, y: 1, width: width - 2, height: 1 };
                pane.render(frame, inner, ctx).unwrap();
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area().width)
            .map(|x| buf[(x, 1)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// The user's tab set (Queue, Playlists, MPD, Radio, Search).
    fn with_five_tabs(mut ctx: Ctx) -> Ctx {
        let mut config = ctx.config.as_ref().clone();
        config.tabs.names = vec![
            "Queue".into(),
            "Playlists".into(),
            "MPD".into(),
            "Radio".into(),
            "Search".into(),
        ];
        ctx.config = std::sync::Arc::new(config);
        ctx
    }

    #[rstest::rstest]
    fn wide_terminal_uses_double_space_separators(ctx: Ctx) {
        let ctx = with_five_tabs(ctx);
        let line = bar_line(&ctx, 110);
        assert!(line.contains("Queue  │  Playlists  │  MPD  •  Radio  •  Search"));
        assert!(line.contains("Help | Settings"));
        // Right group is right-aligned: Settings ends near the right border.
        assert!(line.trim_end().ends_with("Settings"));
    }

    #[rstest::rstest]
    fn narrow_terminal_shrinks_separators_but_keeps_buttons(ctx: Ctx) {
        let ctx = with_five_tabs(ctx);
        let line = bar_line(&ctx, 70);
        assert!(line.contains("Queue"));
        assert!(line.contains("Search"));
        assert!(line.contains("Help | Settings"));
        assert!(line.contains("Settings"));
    }

    #[rstest::rstest]
    fn narrow_terminal_falls_back_to_single_space_separators(ctx: Ctx) {
        let ctx = with_five_tabs(ctx);
        let line = bar_line(&ctx, 60);
        // Double-space separators would overlap the buttons, so the tight
        // single-space look is used.
        assert!(line.contains("Queue │ Playlists"));
        assert!(!line.contains("Queue  │  Playlists"));
    }

    #[rstest::rstest]
    fn hidden_radio_tab_is_omitted(ctx: Ctx) {
        let mut ctx = with_five_tabs(ctx);
        let mut config = ctx.config.as_ref().clone();
        config.ui.show_radio_tab = false;
        ctx.config = std::sync::Arc::new(config);
        let line = bar_line(&ctx, 110);
        assert!(!line.contains("Radio"));
        assert!(line.contains("Queue"));
        assert!(line.contains("MPD"));
    }

    /// The clickable areas must line up exactly with the drawn labels,
    /// otherwise the buttons are unclickable.
    #[rstest::rstest]
    fn click_areas_match_drawn_labels(ctx: Ctx) {
        let ctx = with_five_tabs(ctx);
        let backend = TestBackend::new(110, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut pane = TabsPane::new(&ctx).unwrap();
        terminal
            .draw(|frame| {
                let inner = Rect { x: 1, y: 1, width: 108, height: 1 };
                pane.render(frame, inner, &ctx).unwrap();
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let line: String = (0..buf.area().width)
            .map(|x| buf[(x, 1)].symbol().chars().next().unwrap_or(' '))
            .collect();

        // rfind gives a byte index; the buffer is one cell per char so convert
        // to the char/cell index before comparing with the rect coordinates.
        let cell_idx = |byte: usize| line[..byte].chars().count();
        let help_col = cell_idx(line.rfind("Help").unwrap());
        // The label has a leading space, so the drawn text sits inside the
        // clickable area rather than exactly at its left edge.
        assert!(help_col >= pane.help_area.x as usize);
        assert!(help_col < (pane.help_area.x + pane.help_area.width) as usize);

        let settings_col = cell_idx(line.rfind("Settings").unwrap());
        assert!(settings_col >= pane.settings_area.x as usize);
        assert!(settings_col < (pane.settings_area.x + pane.settings_area.width) as usize);
        // Right group is flush with the right edge of the bar (the border
        // occupies the last cell of the full-width buffer).
        assert_eq!(
            pane.settings_area.x + pane.settings_area.width,
            buf.area().width - 1
        );
    }
}

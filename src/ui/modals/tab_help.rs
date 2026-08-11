use std::borrow::Cow;

use anyhow::Result;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    macros::constraint,
    style::{Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use super::Modal;
use crate::{
    config::{
        keys::{CommonAction, GlobalAction, KeyConfig, ToDescription},
        tabs::PaneType,
    },
    ctx::Ctx,
    shared::{
        events::AppEvent,
        id::{self, Id},
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{UiAppEvent, input::InputResultEvent},
};

/// Help popup showing the keybindings of the currently opened tab. Pressing
/// Tab switches tabs behind the popup and refreshes the list; Esc closes it.
#[derive(Debug)]
pub struct TabHelpModal {
    id: Id,
    list_state: ListState,
    rows: Vec<(String, String, Cow<'static, str>)>,
    title: String,
    list_area: Rect,
    built_for: Option<crate::config::tabs::TabName>,
    /// True shows only the basic navigation keys; false shows every control.
    basic: bool,
    basic_area: Rect,
    advanced_area: Rect,
}

impl TabHelpModal {
    pub fn new(ctx: &Ctx) -> Self {
        let mut modal = Self {
            id: id::new(),
            list_state: ListState::default(),
            rows: Vec::new(),
            title: String::new(),
            list_area: Rect::default(),
            built_for: None,
            basic: true,
            basic_area: Rect::default(),
            advanced_area: Rect::default(),
        };
        modal.rebuild(ctx);
        modal
    }

    /// Collect the keybindings relevant to the active tab: global +
    /// navigation always, plus the pane-specific map of the tab's panes.
    fn rebuild(&mut self, ctx: &Ctx) {
        let keybinds: &KeyConfig = &ctx.config.keybinds;
        let tab = ctx
            .config
            .tabs
            .tabs
            .get(&ctx.active_tab)
            .map(|tab| &tab.panes);
        let mut include_directories = false;
        let mut include_queue = false;
        if let Some(panes) = tab {
            for pane in panes.panes_iter() {
                match pane.pane {
                    PaneType::Directories { .. }
                    | PaneType::Playlists { .. }
                    | PaneType::Radio { .. }
                    | PaneType::Jellyfin { .. } => {
                        include_directories = true;
                    }
                    PaneType::Queue | PaneType::QueueHeader() => include_queue = true,
                    _ => {}
                }
            }
        }

        if self.basic {
            self.rows = basic_rows();
        } else {
            let mut rows: Vec<(String, String, Cow<'static, str>)> = Vec::new();
            push_section(&mut rows, "Global", &keybinds.global);
            push_section(&mut rows, "Navigation", &keybinds.navigation);
            if include_directories {
                push_section(&mut rows, "Regions / browser", &keybinds.directories);
            }
            if include_queue {
                push_section(&mut rows, "Queue", &keybinds.queue);
            }
            self.rows = rows;
        }
        if !self.rows.is_empty() {
            self.list_state.select(Some(0));
        }
        self.title = format!(
            " Help — {} — {} ",
            ctx.active_tab,
            if self.basic { "Basic" } else { "Advanced" }
        );
        self.built_for = Some(ctx.active_tab.clone());
    }

    /// The name of the tab `dir` steps away from the current one, skipping
    /// tabs hidden via the Settings panel.
    fn tab_at(&self, ctx: &Ctx, dir: i64) -> Option<crate::config::tabs::TabName> {
        let visible: Vec<_> = ctx
            .config
            .tabs
            .names
            .iter()
            .filter(|name| !ctx.config.is_tab_hidden(name))
            .collect();
        if visible.is_empty() {
            return None;
        }
        let idx = visible.iter().position(|t| *t == &ctx.active_tab).unwrap_or(0) as i64;
        let next = (idx + dir).rem_euclid(visible.len() as i64) as usize;
        Some(visible[next].clone())
    }

    fn switch_tab(&mut self, ctx: &mut Ctx, dir: i64) -> Result<()> {
        let Some(next) = self.tab_at(ctx, dir) else { return Ok(()) };
        ctx.app_event_sender.send(AppEvent::UiEvent(UiAppEvent::ChangeTab(next)))?;
        self.rebuild(ctx);
        ctx.render()?;
        Ok(())
    }
}

/// The curated list of basic navigation keys shown by default.
fn basic_rows() -> Vec<(String, String, Cow<'static, str>)> {
    vec![
        ("Navigation".to_owned(), String::new(), Cow::Borrowed("")),
        ("w / ↑".to_owned(), "Up".to_owned(), "move up".into()),
        ("s / ↓".to_owned(), "Down".to_owned(), "move down".into()),
        ("d / →".to_owned(), "Play".to_owned(), "play the highlighted track / station".into()),
        ("Shift+W/S / Shift+↑↓".to_owned(), "Select".to_owned(), "select a range of rows".into()),
        ("Enter".to_owned(), "Confirm".to_owned(), "open the context menu / open a region".into()),
        ("Space".to_owned(), "Play/Pause".to_owned(), "play or pause playback".into()),
        ("Tab".to_owned(), "NextTab".to_owned(), "switch to the next tab".into()),
        ("Shift+E".to_owned(), "NextTab".to_owned(), "switch to the next tab".into()),
        ("Shift+Q".to_owned(), "PreviousTab".to_owned(), "switch to the previous tab".into()),
        ("Esc".to_owned(), "Close / Settings".to_owned(), "close a menu, otherwise open settings".into()),
        ("q".to_owned(), "Quit".to_owned(), "exit rmpc".into()),
    ]
}

fn push_section<K, A>(rows: &mut Vec<(String, String, Cow<'static, str>)>, name: &str, map: &std::collections::HashMap<K, A>)
where
    K: std::fmt::Display,
    A: std::fmt::Display + ToDescription,
{
    let mut section: Vec<(String, String, Cow<'static, str>)> = map
        .iter()
        .map(|(key, action)| (key.to_string(), action.to_string(), action.to_description()))
        .collect();
    if section.is_empty() {
        return;
    }
    section.sort_by(|a, b| a.0.cmp(&b.0));
    rows.push((name.to_owned(), String::new(), Cow::Borrowed("")));
    rows.extend(section);
}

impl Modal for TabHelpModal {
    fn id(&self) -> Id {
        self.id
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        // The tab may have switched behind the popup; refresh the rows when
        // the active tab changed.
        if self.built_for.as_ref() != Some(&ctx.active_tab) {
            self.rebuild(ctx);
        }
        // The help popup is compact (the previous settings size).
        let popup_area = frame.area().centered(constraint!(==46), constraint!(==16));
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame.render_widget(Block::default().style(Style::default().bg(bg_color)), popup_area);
        }

        let base = ctx
            .config
            .theme
            .text_color
            .map_or_else(Style::default, |c| Style::default().fg(c));
        let dim = base.add_modifier(Modifier::DIM);
        let active = ctx.config.theme.current_item_style;
        let group = ctx.config.theme.preview_metadata_group_style;

        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(ctx.config.as_border_style())
            .title_alignment(ratatui::prelude::Alignment::Center)
            .title(self.title.clone());
        let inner = block.inner(popup_area);

        // Basic | Advanced toggle header.
        let margin = Margin { horizontal: 1, vertical: 0 };
        let [toggle_area, body_area, footer_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Percentage(100), Constraint::Length(1)])
                .areas(inner.inner(margin));
        let basic_label = Span::styled(
            "  Basic  ",
            if self.basic { active } else { dim },
        );
        let advanced_label = Span::styled(
            "  Advanced  ",
            if self.basic { dim } else { active },
        );
        let toggle_x = toggle_area.x;
        let basic_rect = Rect { x: toggle_x, y: toggle_area.y, width: 10, height: 1 };
        frame.render_widget(
            Paragraph::new(Line::from(vec![basic_label, Span::raw("│"), advanced_label])),
            toggle_area,
        );
        self.basic_area = basic_rect;
        self.advanced_area = Rect { x: toggle_x + 11, y: toggle_area.y, width: 12, height: 1 };
        let _ = toggle_x;

        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|(key, action, description)| {
                if action.is_empty() {
                    // Section header.
                    ListItem::new(Line::styled(format!(" {key} "), group))
                } else {
                    ListItem::new(Line::from(vec![
                        Span::styled(key.clone(), base),
                        Span::raw("  "),
                        Span::styled(action.clone(), dim),
                        Span::raw("  "),
                        Span::raw(description.clone()),
                    ]))
                }
            })
            .collect();

        let list_area = body_area;
        ratatui::widgets::StatefulWidget::render(
            List::new(items).highlight_style(ctx.config.theme.current_item_style),
            list_area,
            frame.buffer_mut(),
            &mut self.list_state,
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("a", base),
                Span::styled("  basic/advanced · ", dim),
                Span::styled("Tab", base),
                Span::styled("  switch tab · ", dim),
                Span::styled("Esc", base),
                Span::styled("  close", dim),
            ]))
            .style(dim),
            footer_area,
        );
        frame.render_widget(block, popup_area);
        self.list_area = list_area;
        Ok(())
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &Ctx) -> Result<()> {
        let _ = (kind, ctx);
        Ok(())
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = key.claim_common() {
            match action {
                CommonAction::Close => return self.hide(ctx),
                CommonAction::AddOptions { .. } => {
                    self.basic = !self.basic;
                    self.rebuild(ctx);
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::Up | CommonAction::Down => {
                    let dir = if matches!(action, CommonAction::Up) { -1 } else { 1 };
                    if !self.rows.is_empty() {
                        let current = self.list_state.selected().unwrap_or(0) as i64;
                        let next = (current + dir).clamp(0, self.rows.len() as i64 - 1) as usize;
                        self.list_state.select(Some(next));
                        ctx.render()?;
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
        if let Some(action) = key.claim_global() {
            match action {
                GlobalAction::NextTab => return self.switch_tab(ctx, 1),
                GlobalAction::PreviousTab => return self.switch_tab(ctx, -1),
                _ => {}
            }
        }
        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        if matches!(event.kind, MouseEventKind::LeftClick | MouseEventKind::DoubleClick) {
            let position: ratatui::layout::Position = event.into();
            if self.basic_area.contains(position) && !self.basic {
                self.basic = true;
                self.rebuild(ctx);
                ctx.render()?;
            } else if self.advanced_area.contains(position) && self.basic {
                self.basic = false;
                self.rebuild(ctx);
                ctx.render()?;
            }
        }
        if self.list_area.contains(event.into())
            && matches!(event.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown)
        {
            let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
            if !self.rows.is_empty() {
                let current = self.list_state.selected().unwrap_or(0) as i64;
                let next = (current + dir).clamp(0, self.rows.len() as i64 - 1) as usize;
                self.list_state.select(Some(next));
                ctx.render()?;
            }
        }
        Ok(())
    }
}

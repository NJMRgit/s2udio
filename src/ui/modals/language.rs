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
    config::keys::CommonAction,
    ctx::Ctx,
    shared::{
        id::{self, Id},
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
    },
};

/// What choosing a row does: a preference entry (system language / hidden)
/// or a concrete language code. Header rows carry `None`.
#[derive(Debug, Clone)]
enum LangChoice {
    System,
    Hidden,
    Custom(String),
}

/// The mpv language picker opened by the controls bar's `[Audio]` / `[Sub]`
/// buttons. Styled like the help popup: a compact centered box, section
/// headers (Preference / Languages) in the group style and a scrollable
/// list; Enter/double-click chooses, Esc closes, wheel scrolls.
#[derive(Debug)]
pub struct LanguageModal {
    id: Id,
    list_state: ListState,
    rows: Vec<(String, Option<LangChoice>)>,
    title: String,
    /// Whether this is the audio-language picker (subtitles otherwise).
    audio: bool,
    list_area: Rect,
}

impl LanguageModal {
    pub fn new(ctx: &Ctx, title: &str, audio: bool) -> Self {
        let mut rows: Vec<(String, Option<LangChoice>)> = Vec::new();
        rows.push(("Preference".to_owned(), None));
        if audio {
            rows.push(("System language".to_owned(), Some(LangChoice::System)));
        } else {
            rows.push(("Hidden".to_owned(), Some(LangChoice::Hidden)));
            rows.push(("System language".to_owned(), Some(LangChoice::System)));
        }
        rows.push(("Languages".to_owned(), None));
        rows.extend(
            crate::ui::modals::settings::language_options().iter().map(|(name, code)| {
                (name.to_string(), Some(LangChoice::Custom((*code).to_owned())))
            }),
        );

        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            id: id::new(),
            list_state,
            rows,
            title: format!(" {title} "),
            audio,
            list_area: Rect::default(),
        }
    }

    /// Apply the highlighted choice (persist the preference + re-select the
    /// matching track on the running mpv) and close the popup.
    fn apply(&mut self, ctx: &mut Ctx) -> Result<()> {
        use crate::config::mpv::{MpvAudioLang, MpvSubtitleMode};
        let Some((_, Some(choice))) = self.rows.get(self.list_state.selected().unwrap_or(0)) else {
            return Ok(());
        };
        let mut config = ctx.config.as_ref().clone();
        match choice {
            LangChoice::System => {
                if self.audio {
                    config.mpv.audio_lang = MpvAudioLang::System;
                } else {
                    config.mpv.subtitles = MpvSubtitleMode::SystemLanguage;
                }
            }
            LangChoice::Hidden => {
                config.mpv.subtitles = MpvSubtitleMode::Hidden;
            }
            LangChoice::Custom(lang) => {
                if self.audio {
                    config.mpv.audio_lang = MpvAudioLang::Custom { lang: lang.clone() };
                } else {
                    config.mpv.subtitles = MpvSubtitleMode::Custom { lang: lang.clone() };
                }
            }
        }
        ctx.config = std::sync::Arc::new(config);
        // Persist (a restart keeps the choice) and re-select the matching
        // track on the running mpv instance.
        crate::core::mpv::persist_mpv_prefs(ctx);
        crate::core::mpv::apply_mpv_prefs_live(ctx);
        self.hide(ctx)?;
        Ok(())
    }
}

impl Modal for LanguageModal {
    fn id(&self) -> Id {
        self.id
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        // The language popup is compact, like the help popup.
        let popup_area = frame.area().centered(constraint!(==46), constraint!(==18));
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame.render_widget(Block::default().style(Style::default().bg(bg_color)), popup_area);
        }

        let base =
            ctx.config.theme.text_color.map_or_else(Style::default, |c| Style::default().fg(c));
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

        let margin = Margin { horizontal: 1, vertical: 0 };
        let [body_area, footer_area] =
            Layout::vertical([Constraint::Percentage(100), Constraint::Length(1)])
                .areas(inner.inner(margin));

        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|(label, choice)| {
                if choice.is_none() {
                    // Section header.
                    ListItem::new(Line::styled(format!(" {label} "), group))
                } else {
                    ListItem::new(Line::from(Span::styled(label.clone(), base)))
                }
            })
            .collect();

        ratatui::widgets::StatefulWidget::render(
            List::new(items).highlight_style(active),
            body_area,
            frame.buffer_mut(),
            &mut self.list_state,
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Enter", base),
                Span::styled("  choose · ", dim),
                Span::styled("Esc", base),
                Span::styled("  close", dim),
            ]))
            .style(dim),
            footer_area,
        );
        frame.render_widget(block, popup_area);
        self.list_area = body_area;
        Ok(())
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = key.claim_common() {
            match action {
                CommonAction::Close => return self.hide(ctx),
                CommonAction::Confirm => return self.apply(ctx),
                CommonAction::Up | CommonAction::Down => {
                    let dir = if matches!(action, CommonAction::Up) { -1 } else { 1 };
                    let current = self.list_state.selected().unwrap_or(0) as i64;
                    let next = (current + dir).clamp(0, self.rows.len() as i64 - 1) as usize;
                    self.list_state.select(Some(next));
                    ctx.render()?;
                    return Ok(());
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        if !self.list_area.contains(event.into()) {
            return Ok(());
        }
        let clicked_row: usize = event.y.saturating_sub(self.list_area.y).into();
        let Some(idx) = self.list_state.offset().checked_add(clicked_row) else {
            return Ok(());
        };
        if idx >= self.rows.len() {
            return Ok(());
        }
        match event.kind {
            MouseEventKind::LeftClick => {
                self.list_state.select(Some(idx));
                ctx.render()?;
            }
            MouseEventKind::DoubleClick => {
                self.list_state.select(Some(idx));
                return self.apply(ctx);
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                let current = self.list_state.selected().unwrap_or(0) as i64;
                let next = (current + dir).clamp(0, self.rows.len() as i64 - 1) as usize;
                self.list_state.select(Some(next));
                ctx.render()?;
            }
            _ => {}
        }
        Ok(())
    }
}

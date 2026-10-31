use anyhow::Result;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Modifier, Style, Stylize},
    symbols,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::{
    model::{ContentRow, Section, SettingsFocus},
    SettingsModal,
};
use crate::{
    config::state::AppStateFile,
    ctx::Ctx,
    shared::macros::status_warn,
    ui::modals::remap_keys,
};

pub(super) fn render(
    modal: &mut SettingsModal,
    frame: &mut Frame,
    ctx: &mut Ctx,
) -> Result<()> {
    if let Some(lang) = ctx.mpv_custom_subtitle_lang.borrow_mut().take() {
        modal.mpv_subtitles_pending = crate::config::mpv::MpvSubtitleMode::Custom {
            lang: lang.clone(),
        };
        modal.mpv_custom_lang = lang;
    }
    if let Some(lang) = ctx.mpv_custom_audio_lang.borrow_mut().take() {
        modal.mpv_audio_pending = crate::config::mpv::MpvAudioLang::Custom {
            lang: lang.clone(),
        };
        modal.mpv_custom_lang = lang;
    }
    if let Some(proxy) = ctx.torrent_socks_proxy_input.borrow_mut().take() {
        modal.torrent_socks_proxy_pending = proxy;
    }
    if let Some(chord) = ctx.qat_hotkey_input.borrow_mut().take() {
        match crate::config::daemon::normalize_hotkey(&chord) {
            Some(chord) => modal.qat_hotkey_pending = chord,
            None => status_warn!("The QAT hotkey cannot be empty or contain quotes/newlines"),
        }
    }
    modal.refresh_rows(ctx);
    modal.library_path = AppStateFile::load().mpd_library_path.unwrap_or_default();
    let popup_area = frame.area();
    frame.render_widget(Clear, popup_area);
    if let Some(bg_color) = ctx.config.theme.modal_background_color {
        frame.render_widget(
            Block::default().style(Style::default().bg(bg_color)),
            popup_area,
        );
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(symbols::border::ROUNDED)
        .border_style(ctx.config.as_border_style())
        .title_alignment(ratatui::prelude::Alignment::Center)
        .title(
            if modal.capturing {
                " Settings — keybinds: press a new key (Esc cancel) ".bold()
            } else if let Some(target) = modal.editing_color {
                format!(
                    " Settings — {}: type a hex color (Enter save, Esc cancel) ",
                    target.name()
                )
                .bold()
            } else {
                " Settings ".bold()
            },
        );
    let inner = block
        .inner(popup_area)
        .inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
    let (base, dim, active) = modal.styles(ctx);
    let [body_area, spacer_area, footer_area] = Layout::vertical([
        Constraint::Percentage(100),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let [top_pad, body_area] = Layout::vertical([Constraint::Length(1), Constraint::Percentage(100)])
        .areas(body_area);
    let _ = (spacer_area, top_pad);
    let [sidebar_area, divider_area, gap_area, content_area] = Layout::horizontal([
        Constraint::Length(20),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Percentage(100),
    ])
    .areas(body_area);
    let _ = gap_area;
    modal.sidebar_areas = Vec::new();
    let mouse = ctx.modal_mouse_pos();
    let sidebar_focused = modal.focus == SettingsFocus::Sidebar;
    let mut side_lines: Vec<Line> = Vec::new();
    for (idx, section) in Section::all().iter().enumerate() {
        let is_active = idx == modal.sidebar_selected;
        let mut style = if is_active && sidebar_focused {
            active
        } else if is_active {
            base.add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        let area = Rect {
            x: sidebar_area.x,
            y: sidebar_area.y + idx as u16,
            width: sidebar_area.width,
            height: 1,
        };
        if mouse.is_some_and(|p| area.contains(p)) {
            style = crate::config::hover_style(style);
        }
        side_lines.push(Line::from(Span::styled(
            format!(" {} {}", if is_active { ">" } else { " " }, section.name()),
            style,
        )));
        modal.sidebar_areas.push(area);
    }
    frame.render_widget(Paragraph::new(side_lines), sidebar_area);
    let visible = (content_area.height as usize).min(modal.rows.len());
    modal.visible_rows = visible;
    let max_scroll = modal.rows.len().saturating_sub(visible);
    modal.scroll = modal.scroll.min(max_scroll);
    if modal.selected < modal.scroll {
        modal.scroll = modal.selected;
    } else if modal.selected >= modal.scroll + visible {
        modal.scroll = modal.selected.saturating_add(1).saturating_sub(visible);
    }
    modal.key_col_w = 0;
    modal.action_col_w = 0;
    modal.desc_col_w = 0;
    if modal.section == Section::Keybinds {
        for row in &modal.rows {
            if let ContentRow::KeyItem(item) = row {
                modal.key_col_w = modal
                    .key_col_w
                    .max(remap_keys::key_display(&item.keys).chars().count());
                modal.action_col_w = modal.action_col_w.max(item.display.chars().count());
                modal.desc_col_w = modal
                    .desc_col_w
                    .max(remap_keys::remap_description(&item.action).chars().count());
            }
        }
        let content_w = content_area.width as usize;
        modal.key_col_w = (modal.key_col_w + 1).min(15).max(5);
        modal.action_col_w = (modal.action_col_w + 1).min(21).max(7);
        modal.desc_col_w = (modal.desc_col_w + 1)
            .min(
                content_w
                    .saturating_sub(modal.key_col_w + modal.action_col_w + 16)
                    .max(10),
            )
            .max(10);
    }
    modal.row_areas = Vec::new();
    modal.click_targets = Vec::new();
    let mut lines: Vec<Line> = Vec::new();
    for idx in modal.scroll..modal.scroll + visible {
        let row = &modal.rows[idx];
        let y = content_area.y + (idx - modal.scroll) as u16;
        let row_area = Rect {
            x: content_area.x,
            y,
            width: content_area.width,
            height: 1,
        };
        modal.row_areas.push((idx, row_area));
        let hovered = mouse.is_some_and(|p| row_area.contains(p));
        let mut targets = Vec::new();
        let content_selected = idx == modal.selected && modal.focus == SettingsFocus::Content;
        let line = modal.render_row(
            row,
            content_selected,
            hovered,
            ctx,
            base,
            dim,
            active,
            content_area.x,
            y,
            content_area.width,
            &mut targets,
        );
        modal.click_targets.extend(targets);
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), content_area);
    modal.render_scrollbar(frame, content_area, visible, dim);
    let footer = if modal.capturing {
        " press any key to assign · Esc to cancel "
    } else if modal.adjusting.is_some() {
        " a/← d/→  adjust · Space/Enter  commit · Esc  cancel "
    } else if modal.focus == SettingsFocus::Sidebar {
        " w/s ↑/↓  sidebar · d/→/Enter  open · Esc  close "
    } else {
        " w/s ↑/↓  options · d/→/Enter  toggle · a/←  back to sidebar · Esc  close "
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer, dim))),
        footer_area,
    );
    {
        let buf = frame.buffer_mut();
        for y in divider_area.y..divider_area.bottom() {
            buf.set_string(divider_area.x, y, "│", dim);
        }
    }
    frame.render_widget(block, popup_area);
    Ok(())
}

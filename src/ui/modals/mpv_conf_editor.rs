//! The Settings -> mpv "Edit mpv.conf" in-TUI editor (Round 48).
//!
//! A nano-style text editor popup (arrows / Home / End / PageUp / PageDown,
//! Ctrl-A/E line start/end, Ctrl-K/U cut & uncut, insert / Backspace /
//! Delete / Enter) editing the user's real `~/.config/mpv/mpv.conf`. Save
//! (`Ctrl-O` or the bottom-right Save button) writes the file and closes;
//! `Esc` / `Ctrl-X` / the top-right [x] discard. A short footer notes that
//! a running mpv picks the file up at its next launch — mpv reads
//! mpv.conf only at startup.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::Modal;
use crate::{
    config::keys::CommonAction,
    ctx::Ctx,
    shared::{
        id::{self, Id},
        keys::ActionEvent,
        macros::status_info,
        mouse_event::{MouseEvent, MouseEventKind},
    },
};

/// The mpv config path, tilde-expanded against the run user's HOME (s2udio
/// launches mpv without `--config-dir`, so this is the file mpv reads).
const MPV_CONF: &str = "~/.config/mpv/mpv.conf";

/// The in-TUI nano-style editor for the user's mpv.conf.
#[derive(Debug)]
pub struct MpvConfEditorModal {
    id: Id,
    /// The expanded path (shown in the title).
    path: PathBuf,
    lines: Vec<String>,
    /// Cursor position: (column in chars, row index into `lines`).
    cursor: (usize, usize),
    /// Top visible row of the view.
    scroll: usize,
    /// Cut (Ctrl-K) buffer for Ctrl-U uncut.
    cut_buffer: Option<String>,
    dirty: bool,
    close_area: Rect,
    save_area: Rect,
    body_area: Rect,
}

impl MpvConfEditorModal {
    pub fn new() -> Self {
        let path = crate::config::utils::tilde_expand_path(Path::new(MPV_CONF));
        let lines = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
                if let Some(last) = lines.last() && last.is_empty() {
                    lines.pop();
                }
                if lines.is_empty() {
                    lines.push(String::new());
                }
                lines
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                log::info!(
                    path:? = path;
                    "mpv.conf does not exist yet; starting the editor empty"
                );
                vec![String::new()]
            }
            Err(err) => {
                log::error!(path:?, error:? = err; "Failed to open mpv.conf for editing");
                vec![String::new()]
            }
        };
        Self {
            id: id::new(),
            path,
            lines,
            cursor: (0, 0),
            scroll: 0,
            cut_buffer: None,
            dirty: false,
            close_area: Rect::default(),
            save_area: Rect::default(),
            body_area: Rect::default(),
        }
    }

    fn view_height(&self) -> usize {
        self.body_area.height.max(1) as usize
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines
            .get(row)
            .map(|l| l.chars().count())
            .unwrap_or(0)
    }

    fn clamp_cursor_col(&mut self) {
        let len = self.line_len(self.cursor.1);
        self.cursor.0 = self.cursor.0.min(len);
    }

    /// Keep the cursor row inside the viewport and the viewport sane.
    fn keep_cursor_visible(&mut self) {
        let view = self.view_height();
        if self.cursor.1 < self.scroll {
            self.scroll = self.cursor.1;
        } else if self.cursor.1 >= self.scroll.saturating_add(view) {
            self.scroll = self.cursor.1.saturating_sub(view).saturating_add(1);
        }
        self.scroll = self.scroll.min(self.lines.len().max(1) - 1);
    }

    fn move_cursor(&mut self, dx: isize, dy: isize) {
        let row = (self.cursor.1 as isize + dy).clamp(0, self.lines.len() as isize - 1) as usize;
        self.cursor.1 = row;
        if dy == 0 {
            self.cursor.0 = (self.cursor.0 as isize + dx)
                .clamp(0, self.line_len(row) as isize) as usize;
        } else {
            // Vertical moves keep the column, clamped to the target line.
            self.clamp_cursor_col();
        }
        self.keep_cursor_visible();
    }

    fn insert_char(&mut self, c: char) {
        self.dirty = true;
        let (col, row) = self.cursor;
        let line = &mut self.lines[row];
        let mut chars: Vec<char> = line.chars().collect();
        chars.insert(col.min(chars.len()), c);
        *line = chars.into_iter().collect();
        self.cursor.0 += 1;
    }

    fn backspace(&mut self) {
        let (col, row) = self.cursor;
        if col > 0 {
            self.dirty = true;
            let line = &mut self.lines[row];
            let mut chars: Vec<char> = line.chars().collect();
            chars.remove(col - 1);
            *line = chars.into_iter().collect();
            self.cursor.0 -= 1;
        } else if row > 0 {
            // Join with the previous line.
            self.dirty = true;
            let removed = self.lines.remove(row);
            self.cursor.1 = row - 1;
            let prev_len = self.line_len(self.cursor.1);
            self.lines[self.cursor.1].push_str(&removed);
            self.cursor.0 = prev_len;
        }
    }

    fn delete(&mut self) {
        let (col, row) = self.cursor;
        let len = self.line_len(row);
        if col < len {
            self.dirty = true;
            let line = &mut self.lines[row];
            let mut chars: Vec<char> = line.chars().collect();
            chars.remove(col);
            *line = chars.into_iter().collect();
        } else if row + 1 < self.lines.len() {
            self.dirty = true;
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        }
        self.keep_cursor_visible();
    }

    fn enter(&mut self) {
        self.dirty = true;
        let (col, row) = self.cursor;
        let line = &mut self.lines[row];
        let split_at = col.min(line.chars().count());
        let chars: Vec<char> = line.chars().collect();
        let rest: String = chars.into_iter().skip(split_at).collect();
        let head: String = line.chars().take(split_at).collect();
        *line = head;
        self.lines.insert(row + 1, rest);
        self.cursor = (0, row + 1);
        self.keep_cursor_visible();
    }

    fn cut_line(&mut self) {
        // Ctrl-K: cut the whole line (nano joins subsequent presses; here a
        // single-line buffer, each press replaces the previous cut).
        let (_, row) = self.cursor;
        self.dirty = true;
        self.cut_buffer = Some(self.lines[row].clone());
        if self.lines.len() > 1 {
            self.lines.remove(row);
            if self.cursor.1 >= self.lines.len() {
                self.cursor.1 = self.lines.len() - 1;
            }
        } else {
            self.lines[0].clear();
        }
        self.cursor.0 = 0;
        self.keep_cursor_visible();
    }

    fn uncut(&mut self) {
        // Ctrl-U: paste the cut line above the cursor row.
        if let Some(text) = self.cut_buffer.take() {
            self.dirty = true;
            self.lines.insert(self.cursor.1, text);
            self.keep_cursor_visible();
        }
    }

    fn page(&mut self, dir: isize) {
        let (_, row) = self.cursor;
        let view = self.view_height().saturating_sub(1).max(1);
        let target = (row as isize + dir * view as isize)
            .clamp(0, self.lines.len() as isize - 1) as usize;
        self.cursor.1 = target;
        if dir < 0 {
            self.scroll = self.cursor.1.saturating_sub(view.saturating_sub(1));
        } else {
            self.scroll = self.cursor.1;
        }
        self.keep_cursor_visible();
    }

    fn scroll_view(&mut self, dir: isize) {
        let view = self.view_height().max(1);
        let max_scroll = self.lines.len().max(1) - 1;
        let target = (self.scroll as isize + dir * view as isize).clamp(0, max_scroll as isize);
        self.scroll = target as usize;
        // Clamp so the cursor cannot be pushed out of view.
        if self.cursor.1 < self.scroll {
            self.cursor.1 = self.scroll;
        } else if self.cursor.1 >= self.scroll + view {
            self.cursor.1 = self.scroll + view - 1;
        }
    }

    fn save(&mut self, ctx: &Ctx) -> Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create {}", parent.display())
            })?;
        }
        let mut text = self.lines.join("\n");
        text.push('\n');
        std::fs::write(&self.path, text)
            .with_context(|| format!("Failed to write {}", self.path.display()))?;
        self.dirty = false;
        status_info!("Saved {}", self.path.display());
        self.hide(ctx)?;
        Ok(())
    }

    fn discard(&mut self, ctx: &Ctx) -> Result<()> {
        if self.dirty {
            log::info!(path:? = self.path; "mpv.conf edits discarded");
        }
        self.hide(ctx)
    }
}

impl Modal for MpvConfEditorModal {
    fn id(&self) -> Id {
        self.id
    }

    /// Right-click does not close the editor: an accidental right-click
    /// must never discard unsaved edits (Esc / [x] are the discard paths).
    fn right_click_closes(&self) -> bool {
        false
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        let area = frame.area();
        // Margins of ~1/5 of the window per side: the popup fills roughly
        // 3/5 of the screen in each direction (Round 48 spec).
        let width = area.width.saturating_mul(3) / 5;
        let height = area.height.saturating_mul(3) / 5;
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let popup = Rect { x, y, width: width.max(1), height: height.max(1) };
        frame.render_widget(Clear, popup);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame.render_widget(Block::default().style(Style::default().bg(bg_color)), popup);
        }

        let base =
            ctx.config.theme.text_color.map_or_else(Style::default, |c| Style::default().fg(c));
        let dim = base.add_modifier(Modifier::DIM);

        let title = if self.dirty {
            format!(" mpv.conf editor — {} ● ", self.path.display())
        } else {
            format!(" mpv.conf editor — {} ", self.path.display())
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(ctx.config.as_border_style())
            .title(title);
        let inner = block.inner(popup);
        let [top_area, body_area, footer_area] = {
            let rows = popup.height.saturating_sub(2);
            let layout = ratatui::layout::Layout::vertical([
                ratatui::layout::Constraint::Length(1),
                ratatui::layout::Constraint::Length(rows.saturating_sub(2)),
                ratatui::layout::Constraint::Length(1),
            ]);
            layout.areas(inner)
        };
        self.close_area = Rect {
            x: top_area.right().saturating_sub(4),
            y: top_area.y,
            width: 4,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(vec![
            Span::raw(" ".repeat(top_area.width as usize - 4)),
            Span::styled("[x]", dim),
        ])), top_area);

        self.body_area = body_area;
        self.view_height();
        // Clamp the cursor to the content and keep it in view.
        self.clamp_cursor_col();
        self.keep_cursor_visible();

        let view = self.view_height();
        let visible_start = self.scroll.min(self.lines.len().max(1) - 1);
        let cursor_style = ctx
            .config
            .theme
            .current_item_style
            .add_modifier(Modifier::REVERSED);
        let mut rows: Vec<Line<'static>> = Vec::with_capacity(view);
        let body_width = body_area.width.saturating_sub(1) as usize;
        for i in 0..view {
            let Some(line) = self.lines.get(visible_start + i) else {
                break;
            };
            if visible_start + i == self.cursor.1 {
                let col = self.cursor.0.min(line.chars().count());
                let before: String = line.chars().take(col).collect();
                let rest: String = line.chars().skip(col).collect();
                let (at, after) = match rest.chars().next() {
                    Some(ch) => (ch.to_string(), rest.chars().skip(1).collect::<String>()),
                    None => (String::new(), rest),
                };
                let at = if at.is_empty() { " ".to_owned() } else { at };
                let mut spans = vec![Span::raw(truncate(&before, body_width))];
                let remaining = body_width.saturating_sub(before.chars().count());
                spans.push(Span::styled(
                    truncate(&at, remaining.max(1).min(1)),
                    cursor_style,
                ));
                let after_w = body_width
                    .saturating_sub(before.chars().count())
                    .saturating_sub(1);
                spans.push(Span::raw(truncate(&after, after_w)));
                rows.push(Line::from(spans));
            } else {
                rows.push(Line::from(truncate(line, body_width)));
            }
        }
        frame.render_widget(
            Paragraph::new(Text::from(rows)).style(base),
            body_area,
        );
        frame.render_widget(Block::default(), body_area);

        let hint = " Ctrl-O save · Esc discard · restart mpv applies edits ";
        let save_area = Rect {
            x: footer_area.right().saturating_sub(7),
            y: footer_area.y,
            width: 7,
            height: 1,
        };
        self.save_area = save_area;
        let mut footer = vec![Span::styled(hint, dim)];
        let save_w = footer_area.width as usize;
        let hint_w = hint.chars().count();
        if hint_w < save_w.saturating_sub(7) {
            footer.push(Span::raw(" ".repeat(save_w - hint_w - 7)));
            footer.push(Span::styled("[ Save ]", base.add_modifier(Modifier::BOLD)));
        }
        frame.render_widget(Paragraph::new(Line::from(footer)), footer_area);

        // Redraw the block on top so the border never shows content gaps.
        frame.render_widget(block, popup);
        Ok(())
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        // The editor consumes raw keys in handle_raw_key; this is only a
        // safety net for resolved actions that slipped through.
        if let Some(action) = key.claim_common()
            && matches!(action, CommonAction::Close)
        {
            return self.discard(ctx);
        }
        Ok(())
    }

    fn handle_raw_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        ctx: &mut Ctx,
    ) -> Result<bool> {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.discard(ctx)?;
                Ok(true)
            }
            KeyCode::Char(c) if ctrl && c == 'x' => {
                self.discard(ctx)?;
                Ok(true)
            }
            KeyCode::Char(c) if ctrl && c == 'o' => {
                self.save(ctx)?;
                Ok(true)
            }
            KeyCode::Char(c) if ctrl && c == 'k' => {
                self.cut_line();
                Ok(true)
            }
            KeyCode::Char(c) if ctrl && c == 'u' => {
                self.uncut();
                Ok(true)
            }
            KeyCode::Char(c) if ctrl && c == 'a' => {
                self.cursor.0 = 0;
                Ok(true)
            }
            KeyCode::Char(c) if ctrl && c == 'e' => {
                self.cursor.0 = self.line_len(self.cursor.1);
                Ok(true)
            }
            KeyCode::Char(c) if ctrl && c == 'y' => {
                self.page(-1);
                Ok(true)
            }
            KeyCode::Char(c) if ctrl && c == 'v' => {
                self.page(1);
                Ok(true)
            }
            KeyCode::Char(c) if !ctrl => {
                self.insert_char(c);
                Ok(true)
            }
            KeyCode::Backspace => {
                self.backspace();
                Ok(true)
            }
            KeyCode::Delete => {
                self.delete();
                Ok(true)
            }
            KeyCode::Enter => {
                self.enter();
                Ok(true)
            }
            KeyCode::Left => {
                self.move_cursor(-1, 0);
                Ok(true)
            }
            KeyCode::Right => {
                self.move_cursor(1, 0);
                Ok(true)
            }
            KeyCode::Up => {
                self.move_cursor(0, -1);
                Ok(true)
            }
            KeyCode::Down => {
                self.move_cursor(0, 1);
                Ok(true)
            }
            KeyCode::Home => {
                self.cursor.0 = 0;
                Ok(true)
            }
            KeyCode::End => {
                self.cursor.0 = self.line_len(self.cursor.1);
                Ok(true)
            }
            KeyCode::PageUp => {
                self.page(-1);
                Ok(true)
            }
            KeyCode::PageDown => {
                self.page(1);
                Ok(true)
            }
            // Everything else is consumed too: nothing may leak through to
            // the settings modal underneath.
            _ => Ok(true),
        }
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        match event.kind {
            MouseEventKind::LeftClick => {
                let position = event.into();
                if self.close_area.contains(position) {
                    return self.discard(ctx);
                }
                if self.save_area.contains(position) {
                    return self.save(ctx);
                }
                if self.body_area.contains(position) {
                    // Click-to-place the cursor on the clicked line (clamped
                    // to the content; column stays at the text start —
                    // keyboard is the primary cursor driver).
                    if let Some(row) = visible_row_clicked(
                        event.y,
                        self.body_area.y,
                        self.scroll,
                    ) {
                        self.cursor.1 = row.min(self.lines.len().saturating_sub(1));
                        self.cursor.0 = 0;
                    }
                    Ok(())
                } else {
                    Ok(())
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if self.body_area.contains(event.into()) {
                    let dir = if matches!(event.kind, MouseEventKind::ScrollUp) {
                        -1
                    } else {
                        1
                    };
                    self.scroll_view(dir);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Truncate `s` to at most `width` chars, appending an ellipsis when cut
/// (the editor view does not scroll horizontally; mpv.conf lines are
/// option-value pairs and are short in practice).
fn truncate(s: &str, width: usize) -> String {
    let mut chars = s.chars();
    if chars.clone().count() <= width {
        return s.to_owned();
    }
    let take = width.saturating_sub(1);
    let head: String = chars.by_ref().take(take).collect();
    format!("{head}…")
}

/// Map a clicked screen row to an editor line index (`None` below the
/// visible lines).
fn visible_row_clicked(click_y: u16, body_y: u16, scroll: usize) -> Option<usize> {
    let row = usize::from(click_y.saturating_sub(body_y));
    Some(scroll + row)
}

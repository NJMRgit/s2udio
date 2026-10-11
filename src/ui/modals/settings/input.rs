use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{
    model::{AdjustSnapshot, ContentRow, GeneralRow, MpdRow, Section, SettingsFocus},
    SettingsModal,
};
use crate::{
    ctx::Ctx,
    mpd::mpd_client::MpdClient,
    shared::mouse_event::{MouseEvent, MouseEventKind},
    ui::modals::Modal,
};

impl SettingsModal {
    /// w/s: move the sidebar highlight. The right pane follows immediately —
    /// the section under the highlight is the one displayed, so the pane no
    /// longer waits for `d` (user request, round 76).
    pub(super) fn move_sidebar(&mut self, dir: i64, ctx: &mut Ctx) {
        let n = Section::all().len();
        self.sidebar_selected = (self.sidebar_selected as i64 + dir).rem_euclid(n as i64) as usize;
        self.populate(ctx);
    }

    /// d / Enter / sidebar click: populate the right pane with the
    /// highlighted section.
    ///
    /// The cursor lands on the first *option*, not on the section's first
    /// row: every section opens with a header row (`general`, `keybinds`,
    /// `library`, …), and parking the highlight there means `d`+`w`/`s` walk
    /// straight back into the header (user request, round 76).
    pub(super) fn populate(&mut self, ctx: &mut Ctx) {
        self.section = Section::all()[self.sidebar_selected];
        self.refresh_rows(ctx);
        self.selected = self.first_option();
        self.scroll = 0;
        ctx.render().ok();
    }

    /// The first selectable (non-header) content row: where the highlight
    /// lands when the content pane takes focus. `0` when the section is
    /// header-only.
    pub(super) fn first_option(&self) -> usize {
        self.rows.iter().position(|row| !row.is_header()).unwrap_or(0)
    }

    /// ↑/↓: move the content highlight; stops at the section boundaries (the
    /// section is chosen with `d`, not by scrolling).
    pub(super) fn move_selection(&mut self, dir: i64, ctx: &mut Ctx) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len();
        let mut pos = self.selected;
        loop {
            let next = pos as i64 + dir;
            if next < 0 || next >= len as i64 {
                break;
            }
            pos = next as usize;
            if !self.rows[pos].is_header() {
                break;
            }
        }
        if pos != self.selected && !self.rows[pos].is_header() {
            self.selected = pos;
            let visible = self.visible_rows.max(1);
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected >= self.scroll + visible {
                self.scroll = self.selected + 1 - visible;
            }
        }
        ctx.render().ok();
    }

    /// The [-] [+] stepper rows whose controls can be focused with
    /// Space/Enter (a/← and d/→ then adjust, Esc cancels). The cava
    /// device row joins them: Enter (or d/→) focuses its [<] [>] cycle
    /// controls, then a/← and d/→ walk the PipeWire capture list.
    pub(super) fn is_stepper_row(&self, idx: usize) -> bool {
        matches!(
            self.rows.get(idx),
            Some(ContentRow::General(
                GeneralRow::Sensitivity
                    | GeneralRow::Fps
                    | GeneralRow::FreqMin
                    | GeneralRow::FreqMax
                    | GeneralRow::NoiseReduction
                    | GeneralRow::Device
            )) | Some(ContentRow::Mpd(MpdRow::Crossfade))
        )
    }

    /// Enter adjust mode on the selected stepper row: focus its controls and
    /// snapshot the staged values so Esc can cancel the whole session.
    pub(super) fn start_adjust(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.adjust_snapshot = Some(AdjustSnapshot {
            cava: self.cava_pending.clone(),
            crossfade: ctx.status.xfade.unwrap_or(0),
        });
        self.adjusting = Some(self.selected);
        ctx.render()?;
        Ok(())
    }

    /// Leave adjust mode, keeping the adjusted value (Space/Enter).
    pub(super) fn commit_adjust(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.adjusting = None;
        self.adjust_snapshot = None;
        ctx.render()?;
        Ok(())
    }

    /// Leave adjust mode, reverting the staged value to the snapshot (Esc).
    pub(super) fn cancel_adjust(&mut self, ctx: &mut Ctx) -> Result<()> {
        let Some(snapshot) = self.adjust_snapshot.take() else { return Ok(()) };
        self.cava_pending = snapshot.cava;
        ctx.command(move |client| {
            client.crossfade(snapshot.crossfade)?;
            Ok(())
        });
        self.adjusting = None;
        self.refresh_rows(ctx);
        ctx.render()?;
        Ok(())
    }

    pub(super) fn handle_raw_key_event(
        &mut self,
        key: KeyEvent,
        ctx: &mut Ctx,
    ) -> Result<bool> {
        if self.capturing {
            return self.capture_remap(key, ctx);
        }
        if self.editing_jellyfin_row.is_some() {
            use crossterm::event::KeyModifiers as KM;
            match key.code {
                KeyCode::Char(c) if !c.is_control() && key.modifiers == KM::NONE => {
                    self.edit_buffer.push(c);
                    ctx.render()?;
                }
                KeyCode::Backspace => {
                    self.edit_buffer.pop();
                    ctx.render()?;
                }
                KeyCode::Enter => {
                    self.commit_edit_jellyfin(ctx)?;
                }
                KeyCode::Esc => {
                    self.editing_jellyfin_row = None;
                    self.edit_buffer.clear();
                    ctx.render()?;
                }
                _ => {}
            }
            return Ok(true);
        }
        if self.editing_color.is_some() {
            use crossterm::event::KeyModifiers as KM;
            match key.code {
                KeyCode::Char(c) if c.is_ascii_hexdigit() && key.modifiers == KM::NONE => {
                    if self.edit_buffer.chars().count() < 6 {
                        self.edit_buffer.push(c);
                        ctx.render()?;
                    }
                }
                KeyCode::Backspace => {
                    self.edit_buffer.pop();
                    ctx.render()?;
                }
                KeyCode::Enter => {
                    self.commit_edit_color(ctx)?;
                }
                KeyCode::Esc => {
                    self.editing_color = None;
                    self.edit_buffer.clear();
                    ctx.render()?;
                }
                _ => {}
            }
            return Ok(true);
        }
        if self.adjusting.is_some() {
            match key.code {
                KeyCode::Char('a') | KeyCode::Left
                    if key.modifiers.is_empty() || matches!(key.code, KeyCode::Left) =>
                {
                    self.adjust(ctx, -1)?;
                }
                KeyCode::Char('d') | KeyCode::Right
                    if key.modifiers.is_empty() || matches!(key.code, KeyCode::Right) =>
                {
                    self.adjust(ctx, 1)?;
                }
                KeyCode::Enter | KeyCode::Char(' ')
                    if key.modifiers == KeyModifiers::NONE =>
                {
                    self.commit_adjust(ctx)?;
                }
                KeyCode::Esc => {
                    self.cancel_adjust(ctx)?;
                }
                _ => {}
            }
            return Ok(true);
        }
        match key.code {
            KeyCode::Char('w') | KeyCode::Char('s') | KeyCode::Up | KeyCode::Down
                if key.modifiers.is_empty()
                    || matches!(key.code, KeyCode::Up | KeyCode::Down) =>
            {
                let dir = match key.code {
                    KeyCode::Char('w') | KeyCode::Up => -1,
                    _ => 1,
                };
                match self.focus {
                    SettingsFocus::Sidebar => self.move_sidebar(dir, ctx),
                    SettingsFocus::Content => self.move_selection(dir, ctx),
                }
            }
            KeyCode::Char('a') | KeyCode::Left
                if key.modifiers.is_empty() || matches!(key.code, KeyCode::Left) =>
            {
                if self.focus == SettingsFocus::Content {
                    self.focus = SettingsFocus::Sidebar;
                    ctx.render()?;
                }
            }
            KeyCode::Char('d') | KeyCode::Right
                if key.modifiers.is_empty() || matches!(key.code, KeyCode::Right) =>
            {
                match self.focus {
                    SettingsFocus::Sidebar => {
                        self.populate(ctx);
                        self.focus = SettingsFocus::Content;
                    }
                    SettingsFocus::Content => {
                        if self.is_stepper_row(self.selected) {
                            self.start_adjust(ctx)?;
                        } else {
                            self.activate(ctx)?;
                        }
                    }
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') if key.modifiers == KeyModifiers::NONE => {
                match self.focus {
                    SettingsFocus::Sidebar => {
                        self.populate(ctx);
                        self.focus = SettingsFocus::Content;
                    }
                    SettingsFocus::Content => {
                        if self.is_stepper_row(self.selected) {
                            self.start_adjust(ctx)?;
                        } else {
                            self.activate(ctx)?;
                        }
                    }
                }
            }
            KeyCode::Esc => {
                if self.has_changes() {
                    self.prompt_save_or_discard(ctx)?;
                } else {
                    self.hide(ctx)?;
                }
            }
            _ => {}
        }
        Ok(true)
    }

    pub(super) fn handle_mouse_event_input(
        &mut self,
        event: MouseEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
        if matches!(event.kind, MouseEventKind::RightClick) {
            if self.has_changes() {
                self.prompt_save_or_discard(ctx)?;
            } else {
                self.hide(ctx)?;
            }
            return Ok(());
        }
        if matches!(
            event.kind,
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick
        ) {
            if let Some(idx) = self
                .sidebar_areas
                .iter()
                .position(|area| area.contains(event.into()))
            {
                self.adjusting = None;
                self.adjust_snapshot = None;
                self.sidebar_selected = idx;
                self.populate(ctx);
                self.focus = SettingsFocus::Content;
                return Ok(());
            }
            if let Some((_, click)) = self
                .click_targets
                .iter()
                .rev()
                .find(|(area, _)| area.contains(event.into()))
            {
                if let Some((idx, _)) = self
                    .row_areas
                    .iter()
                    .find(|(_, area)| area.contains(event.into()))
                {
                    self.selected = *idx;
                }
                self.adjusting = None;
                self.adjust_snapshot = None;
                self.focus = SettingsFocus::Content;
                return self.do_click(*click, ctx);
            }
        }
        if matches!(event.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
            let dir = if matches!(event.kind, MouseEventKind::ScrollUp) {
                -1
            } else {
                1
            };
            let position: ratatui::layout::Position = event.into();
            if self.sidebar_areas.iter().any(|area| area.contains(position)) {
                self.move_sidebar(dir, ctx);
            } else if self.row_areas.iter().any(|(_, area)| area.contains(position)) {
                self.move_selection(dir, ctx);
            }
        }
        Ok(())
    }
}

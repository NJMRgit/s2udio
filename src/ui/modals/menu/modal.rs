use std::borrow::Cow;

use anyhow::Result;
use itertools::Itertools;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    macros::constraint,
    style::Style,
    symbols::border,
    widgets::{Block, Borders, Clear},
};

use super::{
    Section,
    SectionType,
    input_section::InputSection,
    list_section::ListSection,
    multi_action_section::MultiActionSection,
};
use crate::{
    config::keys::{CommonAction, DirectoriesActions},
    ctx::Ctx,
    shared::{
        id::{self, Id},
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{
        FILTER_PREFIX,
        input::{BufferId, InputResultEvent},
        modals::{Modal, menu::select_section::SelectSection},
    },
};

#[derive(Debug)]
pub struct MenuModal<'a> {
    sections: Vec<SectionType<'a>>,
    sections_labels: Vec<Vec<String>>,
    current_section_idx: usize,
    areas: Vec<Rect>,
    width: u16,
    id: Id,
    filter: Option<String>,
    filter_buffer_id: BufferId,
    title: Option<String>,
    /// When set, opening this modal replaces the open modal carrying the
    /// same replacement id (the paste popup refreshes in place when a
    /// torrent scan completes).
    replacement_id: Option<Cow<'static, str>>,
}

impl Modal for MenuModal<'_> {
    fn id(&self) -> Id {
        self.id
    }

    fn replacement_id(&self) -> Option<&Cow<'static, str>> {
        self.replacement_id.as_ref()
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        let needed_height: usize =
            self.sections.iter().map(|section| section.preferred_height() as usize).sum::<usize>()
                + 1
                + self.sections.len();

        let popup_area =
            frame.area().centered(constraint!(==self.width), constraint!(==needed_height as u16));
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame.render_widget(Block::default().style(Style::default().bg(bg_color)), popup_area);
        }

        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_set(border::ROUNDED)
            .border_style(ctx.config.as_border_style())
            .title_alignment(ratatui::prelude::Alignment::Center);
        if let Some(filter) = self.filter.as_ref() {
            block = block.title(format!(" {FILTER_PREFIX}: {filter} "));
        }

        let content_area = block.inner(popup_area);

        let areas = Layout::vertical(Itertools::intersperse(
            self.sections.iter_mut().map(|s| Constraint::Length(s.preferred_height())),
            Constraint::Length(1),
        ))
        .split(content_area);

        let mut section_idx = 0;
        for (idx, area) in areas.iter().enumerate() {
            if idx % 2 == 0 {
                self.sections[section_idx].render(
                    *area,
                    frame.buffer_mut(),
                    self.filter.as_deref(),
                    ctx,
                );
                self.areas[section_idx] = *area;
                section_idx += 1;
            } else {
                let buf = frame.buffer_mut();
                for x in area.left()..area.right() {
                    buf[(x, area.y)]
                        .set_symbol(ratatui::symbols::border::ROUNDED.horizontal_bottom)
                        .set_style(ctx.config.as_border_style());
                }
            }
        }

        frame.render_widget(block, popup_area);

        Ok(())
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &Ctx) -> Result<()> {
        if ctx.input.is_active(self.filter_buffer_id)
            && let Some(filter) = &mut self.filter
        {
            match kind {
                InputResultEvent::Push => {
                    *filter = ctx.input.value(self.filter_buffer_id);
                    self.first_result(ctx);
                }
                InputResultEvent::Pop => {
                    *filter = ctx.input.value(self.filter_buffer_id);
                }
                InputResultEvent::Confirm => {
                    ctx.input.clear_buffer(self.filter_buffer_id);
                }
                InputResultEvent::Cancel => {
                    self.filter = None;
                    ctx.input.clear_buffer(self.filter_buffer_id);
                }
                InputResultEvent::NoChange => {}
            }
        } else {
            match kind {
                InputResultEvent::Push => {}
                InputResultEvent::Pop => {}
                InputResultEvent::Confirm => {
                    if self.sections[self.current_section_idx].confirm(ctx)? {
                        self.destroy(ctx)?;
                    }
                }
                InputResultEvent::Cancel => {
                    self.sections[self.current_section_idx].unfocus(ctx);
                }
                InputResultEvent::NoChange => {}
            }
        }
        ctx.render()?;
        Ok(())
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = key.claim_common() {
            match action {
                CommonAction::EnterSearch => {
                    ctx.input.insert_mode(self.filter_buffer_id);
                    self.filter = Some(String::new());
                    ctx.render()?;
                }
                CommonAction::Up => {
                    self.prev();
                    ctx.render()?;
                }
                CommonAction::Down => {
                    self.next();
                    ctx.render()?;
                }
                CommonAction::Right => {
                    self.sections[self.current_section_idx].right();
                    ctx.render()?;
                }
                CommonAction::Left => {
                    self.sections[self.current_section_idx].left();
                    ctx.render()?;
                }
                CommonAction::Top => {
                    if self.current_section_idx != 0 {
                        self.sections[self.current_section_idx].unselect(ctx);
                    }
                    self.current_section_idx = 0;
                    self.sections[0].select(0);
                    ctx.render()?;
                }
                CommonAction::Bottom => {
                    let sect_idx = self.sections.len() - 1;
                    let last_sect_item_idx = self.sections[sect_idx].len() - 1;

                    if self.current_section_idx != sect_idx {
                        self.sections[self.current_section_idx].unselect(ctx);
                    }
                    self.current_section_idx = sect_idx;
                    self.sections[sect_idx].select(last_sect_item_idx);
                    ctx.render()?;
                }
                CommonAction::Close => {
                    self.destroy(ctx)?;
                }
                CommonAction::Confirm => {
                    if self.sections[self.current_section_idx].confirm(ctx)? {
                        self.destroy(ctx)?;
                    }
                }
                CommonAction::NextResult => {
                    self.next_result(ctx);
                    ctx.render()?;
                }
                CommonAction::PreviousResult => {
                    self.prev_result(ctx);
                    ctx.render()?;
                }
                _ => {}
            }
        }

        // wasd mirrors the arrows: `d` / `→` (the directories actions in the
        // minimal keybind set) select the highlighted option, like Enter.
        if let Some(action) = key.claim_directories() {
            match action {
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    if self.sections[self.current_section_idx].confirm(ctx)? {
                        self.destroy(ctx)?;
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        match event.kind {
            MouseEventKind::LeftClick => {
                if let Some(idx) = self.section_idx_at_position(event.into()) {
                    if idx != self.current_section_idx {
                        self.sections[self.current_section_idx].unselect(ctx);
                    }
                    self.current_section_idx = idx;
                    self.sections[idx].left_click(event.into(), ctx);
                    ctx.render()?;
                }
            }
            MouseEventKind::DoubleClick => {
                if let Some(idx) = self.section_idx_at_position(event.into()) {
                    self.sections[idx].double_click(event.into(), ctx)?;
                    if ctx.input.is_insert_mode() {
                        ctx.render()?;
                    } else {
                        self.destroy(ctx)?;
                    }
                }
            }
            MouseEventKind::MiddleClick => {}
            MouseEventKind::RightClick => {}
            MouseEventKind::ScrollUp => {
                self.prev();
                ctx.render()?;
            }
            MouseEventKind::ScrollDown => {
                self.next();
                ctx.render()?;
            }
            MouseEventKind::Drag { drag_start_position: _ } => {}
            MouseEventKind::LeftRelease => {}
            MouseEventKind::Moved => {}
        }
        Ok(())
    }
}

impl<'a> MenuModal<'a> {
    pub fn new(_ctx: &Ctx) -> Self {
        Self {
            sections: Vec::default(),
            sections_labels: Vec::default(),
            current_section_idx: 0,
            areas: Vec::new(),
            width: 40,
            id: id::new(),
            filter: None,
            filter_buffer_id: BufferId::new(),
            title: None,
            replacement_id: None,
        }
    }

    /// The replacement id this modal refreshes in place under (see
    /// [`Modal::replacement_id`]).
    pub fn replacement_id(mut self, id: impl Into<Cow<'static, str>>) -> Self {
        self.replacement_id = Some(id.into());
        self
    }

    pub fn destroy(&mut self, ctx: &Ctx) -> Result<()> {
        for s in &mut self.sections {
            s.on_close(ctx)?;
        }
        ctx.input.destroy_buffer(self.filter_buffer_id);
        self.hide(ctx)?;
        Ok(())
    }

    fn next_result(&mut self, ctx: &Ctx) {
        let Some(filter) = self.filter.as_ref() else {
            return;
        };

        let sect_count = self.sections.len();
        let curr_sect_idx = self.current_section_idx;
        for i in curr_sect_idx..sect_count + curr_sect_idx {
            let sect_i = i % sect_count;

            let sect = &self.sections[sect_i];
            let start = sect.selected().map_or(0, |s| s + 1);

            for label_idx in start..sect.len() {
                let label = &self.sections_labels[sect_i][label_idx];
                if label.contains(filter) {
                    if sect_i != self.current_section_idx {
                        self.sections[self.current_section_idx].unselect(ctx);
                    }
                    self.current_section_idx = sect_i;
                    self.sections[sect_i].select(label_idx);
                    return;
                }
            }
        }

        // if nothing was found, try to search the current section again from
        // the start to wrap around inside just the section itself
        let sect = &self.sections[self.current_section_idx];
        for label_idx in 0..sect.len() {
            let label = &self.sections_labels[self.current_section_idx][label_idx];
            if label.contains(filter) {
                self.sections[self.current_section_idx].select(label_idx);
                break;
            }
        }
    }

    fn prev_result(&mut self, ctx: &mut Ctx) {
        let Some(filter) = self.filter.as_ref() else {
            return;
        };

        let sect_count = self.sections.len();
        let curr_sect_idx = self.current_section_idx;
        for i in (0..=sect_count).rev() {
            let sect_i = (i + curr_sect_idx) % sect_count;

            let sect = &self.sections[sect_i];
            let end = sect.selected().unwrap_or(sect.len());

            for label_idx in (0..end).rev() {
                let label = &self.sections_labels[sect_i][label_idx];
                if label.contains(filter) {
                    if sect_i != self.current_section_idx {
                        self.sections[self.current_section_idx].unselect(ctx);
                    }
                    self.current_section_idx = sect_i;
                    self.sections[sect_i].select(label_idx);
                    return;
                }
            }
        }

        // if nothing was found, try to search the current section again from
        // the end to wrap around inside just the section itself
        let sect = &self.sections[self.current_section_idx];
        for label_idx in (0..sect.len()).rev() {
            let label = &self.sections_labels[self.current_section_idx][label_idx];
            if label.contains(filter) {
                self.sections[self.current_section_idx].select(label_idx);
                break;
            }
        }
    }

    fn first_result(&mut self, ctx: &Ctx) {
        let Some(filter) = self.filter.as_ref() else {
            return;
        };

        for sect_i in 0..self.sections_labels.len() {
            for label_idx in 0..self.sections_labels[sect_i].len() {
                let label = &self.sections_labels[sect_i][label_idx];
                if label.contains(filter) {
                    if sect_i != self.current_section_idx {
                        self.sections[self.current_section_idx].unselect(ctx);
                    }
                    self.current_section_idx = sect_i;
                    self.sections[sect_i].select(label_idx);
                    return;
                }
            }
        }
    }

    pub fn width(mut self, width: u16) -> Self {
        self.width = width;
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn build(mut self) -> Self {
        if let Some((i, s)) =
            self.sections.iter_mut().enumerate().find_or_first(|(_, s)| s.len() > 0)
        {
            self.current_section_idx = i;
            s.down();
        }
        self.sections_labels =
            self.sections.iter().fold(Vec::<Vec<String>>::new(), |mut acc, s| {
                acc.push(s.item_labels_iter().map(|l| l.to_lowercase()).collect());
                acc
            });
        self
    }

    pub fn list_section(
        mut self,
        ctx: &Ctx,
        cb: impl FnOnce(ListSection) -> Option<ListSection>,
    ) -> Self {
        let section = ListSection::new(ctx.config.theme.current_item_style);
        let section = cb(section);
        if let Some(mut section) = section {
            section.state.set_content_len(Some(section.items.len()));
            self.sections.push(SectionType::Menu(section));
            self.areas.push(Rect::default());
        }
        self
    }

    pub fn multi_section(
        mut self,
        ctx: &Ctx,
        cb: impl FnOnce(MultiActionSection) -> Option<MultiActionSection<'_>>,
    ) -> Self {
        let section = MultiActionSection::new(ctx.config.theme.current_item_style);
        let section = cb(section);
        if let Some(mut section) = section {
            section.build();
            self.sections.push(SectionType::Multi(section));
            self.areas.push(Rect::default());
        }
        self
    }

    pub fn input_section(
        mut self,
        _ctx: &Ctx,
        label: impl Into<Cow<'a, str>>,
        cb: impl FnOnce(InputSection) -> Option<InputSection<'_>>,
    ) -> Self {
        let section = InputSection::new(label);
        let section = cb(section);
        if let Some(section) = section {
            self.sections.push(SectionType::Input(section));
            self.areas.push(Rect::default());
        }
        self
    }

    pub fn select_section(
        mut self,
        ctx: &Ctx,
        cb: impl FnOnce(SelectSection) -> Option<SelectSection>,
    ) -> Self {
        let section = SelectSection::new(ctx.config.theme.current_item_style);
        let section = cb(section);
        if let Some(mut section) = section {
            section.state.set_content_len(Some(section.items.len()));
            self.sections.push(SectionType::Select(section));
            self.areas.push(Rect::default());
        }
        self
    }

    fn next(&mut self) {
        let result = self.sections[self.current_section_idx].down();
        if !result {
            self.current_section_idx = (self.current_section_idx + 1) % self.sections.len();
            self.sections[self.current_section_idx].down();
        }
    }

    fn prev(&mut self) {
        let result = self.sections[self.current_section_idx].up();
        if !result {
            self.current_section_idx =
                (self.current_section_idx + self.sections.len() - 1) % self.sections.len();
            self.sections[self.current_section_idx].up();
        }
    }

    fn section_idx_at_position(&self, position: Position) -> Option<usize> {
        self.areas.iter().enumerate().find(|(_, a)| a.contains(position)).map(|(i, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        config::keys::DirectoriesActions,
        shared::keys::Actions,
    };

    fn menu_with_items(ctx: &Ctx, labels: &[&str]) -> MenuModal<'static> {
        let labels = labels.to_vec();
        MenuModal::new(ctx)
            .list_section(ctx, move |mut section| {
                for label in labels {
                    section.add_item(label, |_ctx| Ok(()));
                }
                Some(section)
            })
            .build()
    }

    /// `d` (FolderExpand) selects the highlighted option, like Enter: the
    /// menu runs the item's action and closes itself.
    #[test]
    fn d_selects_the_highlighted_option() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut modal = menu_with_items(&ctx, &["Option one", "Option two"]);

        let mut action = ActionEvent::from(Arc::new(vec![Actions::Directories(
            DirectoriesActions::FolderExpand,
        )]));
        modal.handle_key(&mut action, &mut ctx).unwrap();

        match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(id))) => {
                assert_eq!(id, modal.id());
            }
            other => panic!("expected PopModal after selecting with d, got {other:?}"),
        }
    }

    /// `→` (PlayFile) selects the highlighted option too.
    #[test]
    fn right_arrow_selects_the_highlighted_option() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut modal = menu_with_items(&ctx, &["Option one"]);

        let mut action = ActionEvent::from(Arc::new(vec![Actions::Directories(
            DirectoriesActions::PlayFile,
        )]));
        modal.handle_key(&mut action, &mut ctx).unwrap();

        match app_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(crate::AppEvent::UiEvent(crate::ui::UiAppEvent::PopModal(id))) => {
                assert_eq!(id, modal.id());
            }
            other => panic!("expected PopModal after selecting with right arrow, got {other:?}"),
        }
    }

    /// w/s (CommonAction Up/Down) move the highlight and never select.
    #[test]
    fn w_s_move_the_highlight_without_selecting() {
        // The app receiver must stay alive for the render() calls the
        // Up/Down handlers issue.
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut modal = menu_with_items(&ctx, &["Option one", "Option two"]);
        // The first item is highlighted after build().
        assert_eq!(modal.sections[0].selected(), Some(0));

        let mut action =
            ActionEvent::from(Arc::new(vec![Actions::Common(CommonAction::Down)]));
        modal.handle_key(&mut action, &mut ctx).unwrap();
        assert_eq!(modal.sections[0].selected(), Some(1), "s moves down");

        let mut action = ActionEvent::from(Arc::new(vec![Actions::Common(CommonAction::Up)]));
        modal.handle_key(&mut action, &mut ctx).unwrap();
        assert_eq!(modal.sections[0].selected(), Some(0), "w moves up");
    }

    fn test_ctx() -> Ctx {
        crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        )
    }
}

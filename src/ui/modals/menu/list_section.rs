use anyhow::Result;
use enum_map::{Enum, EnumMap};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Position, Rect},
    style::Style,
    text::Text,
    widgets::{ListState, StatefulWidget, Widget},
};

use super::Section;
use crate::{ctx::Ctx, shared::ext::rect::RectExt, ui::dirstack::DirState};

#[derive(derive_more::Debug, Default)]
pub struct ListSection {
    pub items: Vec<MenuItem>,
    pub areas: EnumMap<ListSectionArea, Rect>,
    pub current_item_style: Style,
    max_height: Option<usize>,
    pub state: DirState<ListState>,
    /// Runs once when the modal this section belongs to closes (e.g. the
    /// paste popup clears its scan state when it is dismissed).
    #[debug(skip)]
    on_close: Option<Box<dyn FnOnce(&Ctx) + Send + Sync + 'static>>,
}

#[derive(Copy, Clone, Debug, Enum, Eq, PartialEq, Hash)]
pub enum ListSectionArea {
    List = 0,
    Scrollbar = 1,
}

#[derive(derive_more::Debug)]
pub struct MenuItem {
    pub label: String,
    #[debug(skip)]
    pub on_confirm: Option<Box<dyn FnOnce(&Ctx) -> Result<()> + Send + Sync + 'static>>,
    /// Header rows (e.g. "[Audio]" group labels): rendered dim, skipped by
    /// navigation and never confirmed.
    pub disabled: bool,
}

#[allow(dead_code)]
impl ListSection {
    pub fn new(current_item_style: Style) -> Self {
        Self {
            items: Vec::new(),
            areas: EnumMap::default(),
            current_item_style,
            max_height: None,
            state: DirState::default(),
            on_close: None,
        }
    }

    pub fn item(
        mut self,
        label: impl Into<String>,
        on_confirm: impl FnOnce(&Ctx) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.items.push(MenuItem {
            label: label.into(),
            on_confirm: Some(Box::new(on_confirm)),
            disabled: false,
        });
        self
    }

    pub fn add_item(
        &mut self,
        label: impl Into<String>,
        on_confirm: impl FnOnce(&Ctx) -> Result<()> + Send + Sync + 'static,
    ) -> &mut Self {
        self.items.push(MenuItem {
            label: label.into(),
            on_confirm: Some(Box::new(on_confirm)),
            disabled: false,
        });
        self
    }

    /// A dim, non-selectable header row (e.g. "[Audio]" / "[Video]" group
    /// labels inside one list).
    pub fn header(&mut self, label: impl Into<String>) -> &mut Self {
        self.items.push(MenuItem {
            label: label.into(),
            on_confirm: None,
            disabled: true,
        });
        self
    }

    pub fn add_max_height(&mut self, height: usize) -> &mut Self {
        self.max_height = Some(height);
        self
    }

    /// A cleanup hook run when the modal closes (destroyed, confirmed or
    /// cancelled). Named distinctly from the `Section::on_close` trait
    /// method (which invokes it) so the call sites stay unambiguous.
    pub fn set_on_close(&mut self, f: impl FnOnce(&Ctx) + Send + Sync + 'static) -> &mut Self {
        self.on_close = Some(Box::new(f));
        self
    }

    pub fn select_item_at_position(&mut self, position: Position) {
        if !self.areas[ListSectionArea::List].contains(position) {
            return;
        }

        let clicked_row: usize =
            position.y.saturating_sub(self.areas[ListSectionArea::List].y).into();
        let idx = self.state.get_at_rendered_row(clicked_row);
        self.state.select(idx, 0);
    }
}

impl Section for ListSection {
    fn on_close(&mut self, ctx: &Ctx) -> Result<()> {
        if let Some(f) = self.on_close.take() {
            f(ctx);
        }
        Ok(())
    }

    fn down(&mut self) -> bool {
        let initial_selected = self.state.get_selected();
        let last_selectable = self.items.iter().rposition(|i| !i.disabled).unwrap_or(0);
        // Skip disabled (header) rows.
        let mut guard = 0;
        let mut selected: Option<usize>;
        loop {
            self.state.next(0, false);
            guard += 1;
            selected = self.state.get_selected();
            if selected.is_none() || guard > self.items.len() {
                break;
            }
            if !self.items[selected.unwrap()].disabled {
                break;
            }
        }

        if let Some(init) = initial_selected
            && init == last_selectable
            && selected.is_some()
        {
            let offset = self.state.offset();
            self.state.inner.select(None);
            self.state.set_offset(offset);
            return false;
        }
        true
    }

    fn up(&mut self) -> bool {
        let initial_selected = self.state.get_selected();
        let first_selectable = self.items.iter().position(|i| !i.disabled).unwrap_or(0);
        // Skip disabled (header) rows.
        let mut guard = 0;
        let mut selected: Option<usize>;
        loop {
            self.state.prev(0, true);
            guard += 1;
            selected = self.state.get_selected();
            if selected.is_none() || guard > self.items.len() {
                break;
            }
            if !self.items[selected.unwrap()].disabled {
                break;
            }
        }

        if let Some(init) = initial_selected
            && init == first_selectable
            && selected.is_some()
        {
            self.state.inner.select(None);
            self.state.set_offset(0);
            return false;
        }
        true
    }

    fn selected(&self) -> Option<usize> {
        self.state.get_selected()
    }

    fn select(&mut self, idx: usize) {
        self.state.select(Some(idx), 0);
    }

    fn unselect(&mut self, _ctx: &Ctx) {
        let offset = self.state.offset();
        self.state.inner.select(None);
        self.state.set_offset(offset);
    }

    fn confirm(&mut self, ctx: &Ctx) -> Result<bool> {
        let Some(selected_idx) = self.state.get_selected() else { return Ok(false) };
        if self.items[selected_idx].disabled {
            return Ok(false);
        }
        if let Some(cb) = self.items[selected_idx].on_confirm.take() {
            (cb)(ctx)?;
        }
        Ok(true)
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    fn preferred_height(&self) -> u16 {
        let len = self.items.len();
        self.max_height.map_or(len, |mh| len.min(mh)) as u16
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, filter: Option<&str>, ctx: &Ctx) {
        let should_show_scrollbar = ctx.config.as_styled_scrollbar().is_some()
            && self.max_height.is_some_and(|h| h < self.items.len());

        let [list_area, scrolling_area] = if should_show_scrollbar {
            Layout::horizontal([Constraint::Percentage(100), Constraint::Min(1)]).areas(area)
        } else {
            [area, Rect::default()]
        };
        self.areas[ListSectionArea::List] = list_area;
        self.areas[ListSectionArea::Scrollbar] = scrolling_area;

        let list_area = self.areas[ListSectionArea::List];
        self.state.set_content_and_viewport_len(self.items.len(), list_area.height as usize);
        let mouse = ctx.modal_mouse_pos();
        for (idx, item) in self
            .items
            .iter()
            .enumerate()
            .skip(self.state.offset())
            .take(self.max_height.unwrap_or(usize::MAX))
        {
            let mut text = Text::raw(&item.label);
            let selected = self.state.get_selected().is_some_and(|i| i == idx);

            if item.disabled {
                // Group header row: dim, never highlighted.
                text = text.style(
                    ctx.config
                        .theme
                        .text_color
                        .map_or_else(Style::default, |c| Style::default().fg(c))
                        .add_modifier(ratatui::style::Modifier::DIM),
                );
            } else if selected {
                text = text.style(self.current_item_style);
            } else if let Some(f) = filter
                && item.label.to_lowercase().contains(f)
            {
                text = text.style(ctx.config.theme.highlighted_item_style);
            }
            let idx = idx.saturating_sub(self.state.offset());

            let mut item_area = list_area.shrink_from_top(idx as u16);
            item_area.height = 1;
            // Hovering a clickable menu row gets the same treatment as the
            // queue-list hover (`hovered_item_style`), overriding the
            // selection highlight so the pointer state reads clearly.
            if !item.disabled && mouse.is_some_and(|p| item_area.contains(p)) {
                text = text.style(ctx.config.theme.hovered_item_style);
            }
            text.render(item_area, buf);
        }

        if self.areas[ListSectionArea::Scrollbar].width > 0
            && let Some(scrollbar) = ctx.config.as_styled_scrollbar()
        {
            scrollbar.render(
                self.areas[ListSectionArea::Scrollbar],
                buf,
                self.state.as_scrollbar_state_ref(),
            );
        }
    }

    fn left_click(&mut self, position: Position, _ctx: &Ctx) {
        self.select_item_at_position(position);
    }

    fn double_click(&mut self, _pos: Position, ctx: &Ctx) -> Result<bool> {
        self.confirm(ctx)?;
        Ok(false)
    }

    fn item_labels_iter(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        Box::new(self.items.iter().map(|i| i.label.as_str()))
    }
}

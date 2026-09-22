use std::borrow::Cow;
use anyhow::Result;
use itertools::Itertools;
use crossterm::event::KeyModifiers;
use ratatui::{
    Frame, layout::{Constraint, Layout, Position, Rect},
    macros::constraint, style::Style, symbols::border, widgets::{Block, Borders, Clear},
};
use super::{
    Section, SectionType, input_section::InputSection, list_section::ListSection,
    multi_action_section::MultiActionSection,
};
use crate::{
    config::keys::{CommonAction, DirectoriesActions, GlobalAction},
    ctx::Ctx,
    shared::{
        id::{self, Id},
        keys::{ActionEvent, Actions},
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{
        FILTER_PREFIX, input::{BufferId, InputResultEvent},
        modals::Modal,
    },
};
/// One open submenu level of a [`MenuModal`] (round 78): a single list opened
/// from a row of the level below it. There is only ever ONE window — it shows
/// the popup's own sections, or the deepest open level — so the keyboard and
/// the mouse walk the same tree (the keyboard with `Enter`/`Right`/`Left`, the
/// mouse by clicking rows and buttons).
#[derive(Debug)]
struct SubLevel<'a> {
    /// The level's sections — always exactly one list.
    sections: Vec<SectionType<'a>>,
    /// Per-section areas of the last render (mirrors `MenuModal::areas`).
    areas: Vec<Rect>,
    /// Label of the parent row that opened this level (breadcrumb title).
    label: String,
    /// Row index in the parent level (cursor restore + child-list hand-back).
    parent_row: usize,
}

/// A running drag auto-scroll: the pointer is held past the list's top or
/// bottom edge, so the rows keep scrolling once per frame until it comes back
/// inside (or the button is released).
#[derive(Debug, Clone, Copy)]
struct DragScroll {
    /// Rows per frame — grows with the distance the pointer is past the edge.
    rows: usize,
    /// `true` = scrolling towards the end of the list.
    down: bool,
}

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
    /// Mouse position the popup is anchored at (right-click menus); the
    /// popup's top-left lands under the cursor, clamped into the frame.
    /// `None` keeps the centered placement (keyboard-opened menus and
    /// every menu opened from inside another modal). It also decides the
    /// dismissal gesture: an anchored (mouse-raised) popup is dismissed by a
    /// left click outside it, a centred (keyboard-raised) one only by a right
    /// click or Esc (round 88.1).
    anchor: Option<Position>,
    /// The popup's rect from the most recent render (computed fresh each
    /// frame); the mouse handler uses it to tell a click on the popup from one
    /// on the UI behind it, and `is_empty` (nothing rendered yet) disables the
    /// outside-click dismissal so a click is never judged against a stale
    /// rect (round 88).
    popup_area: Rect,
    /// Open submenu levels below the popup's own sections (round 78). The
    /// popup itself is level 0; empty = it shows its own sections.
    levels: Vec<SubLevel<'a>>,
    /// A running drag auto-scroll (see `DragScroll`).
    drag_scroll: Option<DragScroll>,
}
impl Modal for MenuModal<'_> {
    fn id(&self) -> Id {
        self.id
    }
    fn replacement_id(&self) -> Option<&Cow<'static, str>> {
        self.replacement_id.as_ref()
    }
    fn submenu_path(&self) -> Vec<String> {
        self.selected_row_path()
    }
    fn restore_submenu_path(&mut self, path: &[String], ctx: &mut Ctx) {
        self.restore_row_path(path, ctx);
    }
    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        // A running drag auto-scroll steps once per frame; asking for the next
        // frame keeps it going while the pointer is held past the list's edge.
        if let Some(scroll) = self.drag_scroll {
            if self.active_section_mut().drag_scroll(scroll.rows, scroll.down) {
                let _ = ctx.render();
            } else {
                // The list reached that end: nothing left to scroll.
                self.drag_scroll = None;
            }
        }
        let frame_area = frame.area();
        // Submenu levels never grow past two thirds of the terminal (the
        // chapter picker): their rows scroll instead.
        let max_level_height = (frame_area.height / 3).saturating_mul(2).max(4);
        for level in &mut self.levels {
            for section in &mut level.sections {
                section.cap_window_height(max_level_height);
            }
        }
        // The popup shows its own sections, or the deepest open level. Both
        // input methods use this one centered window (no flyouts).
        let deepest = self.levels.len().checked_sub(1);
        let (needed_height, title) = match deepest {
            Some(idx) => (
                menu_window_height(&self.levels[idx].sections),
                Some(format!(" [{}] ", self.level_path(idx))),
            ),
            None => (menu_window_height(&self.sections), self.title.clone()),
        };
        let popup_area = self
            .anchor
            .map(|anchor| anchor_rect(anchor, self.width, needed_height, frame_area))
            .unwrap_or_else(|| {
                frame_area
                    .centered(constraint!(== self.width), constraint!(== needed_height))
            });
        self.popup_area = popup_area;
        match deepest {
            Some(idx) => {
                let level = &mut self.levels[idx];
                draw_menu_window(
                    frame,
                    ctx,
                    &mut level.sections,
                    &mut level.areas,
                    popup_area,
                    title.as_deref(),
                    None,
                );
            }
            None => draw_menu_window(
                frame,
                ctx,
                &mut self.sections,
                &mut self.areas,
                popup_area,
                title.as_deref(),
                self.filter.as_deref(),
            ),
        }
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
                InputResultEvent::AtStart => {}
                InputResultEvent::CursorLeft => {}
            }
        } else {
            match kind {
                InputResultEvent::Push => {}
                InputResultEvent::Pop => {}
                InputResultEvent::Confirm => {
                    if self.active_section_mut().confirm(ctx)? {
                        self.destroy(ctx)?;
                    }
                }
                InputResultEvent::Cancel => {
                    self.active_section_mut().unfocus(ctx);
                }
                InputResultEvent::NoChange => {}
                InputResultEvent::AtStart => {}
                InputResultEvent::CursorLeft => {}
            }
        }
        ctx.render()?;
        Ok(())
    }
    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        // Any key stops a running drag auto-scroll and ends the drag toggle.
        self.drag_scroll = None;
        self.active_section_mut().end_check_drag();
        // `Space` ticks the focused row of a checkbox list. The shipped (and
        // the user's) keymap binds Space to the global `TogglePause` rather
        // than to `CommonAction::Select`, so both are honoured here.
        let space = key
            .actions
            .iter()
            .any(|action| matches!(action, Actions::Global(GlobalAction::TogglePause)));
        if space && self.active_section_mut().toggle_selected_check() {
            ctx.render()?;
            return Ok(());
        }
        if let Some(action) = key.claim_common() {
            match action {
                CommonAction::EnterSearch => {
                    if self.levels.is_empty() {
                        ctx.input.insert_mode(self.filter_buffer_id);
                        self.filter = Some(String::new());
                    }
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
                    if !self.open_selected_submenu() {
                        if self.active_section_mut().buttons_focused() {
                            // Walk the footer buttons (Download -> Cancel).
                            self.active_section_mut().move_button_focus(true);
                        } else if !self.active_section_mut().focus_buttons() {
                            self.active_section_mut().right();
                        }
                    }
                    ctx.render()?;
                }
                CommonAction::Left => {
                    if self.active_section_mut().buttons_focused() {
                        // Walk back along the buttons; at the first one the
                        // focus returns to the list.
                        self.active_section_mut().move_button_focus(false);
                    } else if self.levels.is_empty() {
                        self.active_section_mut().left();
                    } else {
                        self.close_level();
                    }
                    ctx.render()?;
                }
                CommonAction::Top => {
                    if let Some(level) = self.levels.last_mut() {
                        level.sections[0].select(0);
                    } else {
                        if self.current_section_idx != 0 {
                            self.sections[self.current_section_idx].unselect(ctx);
                        }
                        self.current_section_idx = 0;
                        self.sections[0].select(0);
                    }
                    ctx.render()?;
                }
                CommonAction::Bottom => {
                    if let Some(level) = self.levels.last_mut() {
                        let last = level.sections[0].len().saturating_sub(1);
                        level.sections[0].select(last);
                    } else {
                        let sect_idx = self.sections.len() - 1;
                        let last_sect_item_idx = self.sections[sect_idx].len() - 1;
                        if self.current_section_idx != sect_idx {
                            self.sections[self.current_section_idx].unselect(ctx);
                        }
                        self.current_section_idx = sect_idx;
                        self.sections[sect_idx].select(last_sect_item_idx);
                    }
                    ctx.render()?;
                }
                CommonAction::Close => {
                    // One level back per Esc; the popup itself closes last.
                    if self.levels.is_empty() {
                        self.destroy(ctx)?;
                    } else {
                        self.close_level();
                        ctx.render()?;
                    }
                }
                CommonAction::Confirm => {
                    if self.open_selected_submenu() {
                        ctx.render()?;
                    } else if self.active_section_mut().buttons_focused() {
                        // Enter on a footer button runs it (Download starts
                        // the job, Cancel closes the picker).
                        if self.active_section_mut().activate_focused_button(ctx)?.is_some() {
                            self.destroy(ctx)?;
                        } else {
                            ctx.render()?;
                        }
                    } else if self.active_section_mut().confirm_on_enter(ctx)? {
                        // The multi-stream download picker: its rows are ticked
                        // already, so Enter on the list starts the download
                        // (the activated button always closes the picker).
                        self.destroy(ctx)?;
                    } else if self.active_section_mut().focus_buttons() {
                        // Enter on a row moves the focus onto `Download`; a
                        // second Enter downloads.
                        ctx.render()?;
                    } else if self.active_section_mut().confirm(ctx)? {
                        self.destroy(ctx)?;
                    } else {
                        ctx.render()?;
                    }
                }
                CommonAction::Select => {
                    // Space (when bound): ticks the focused checkbox row.
                    self.active_section_mut().toggle_selected_check();
                    ctx.render()?;
                }
                CommonAction::SelectDown => {
                    // Shift+Down extends the ticked range of a picker.
                    if self.active_section_mut().extend_check_selection(true) {
                        ctx.render()?;
                    }
                }
                CommonAction::SelectUp => {
                    if self.active_section_mut().extend_check_selection(false) {
                        ctx.render()?;
                    }
                }
                CommonAction::NextResult => {
                    if self.levels.is_empty() {
                        self.next_result(ctx);
                        ctx.render()?;
                    }
                }
                CommonAction::PreviousResult => {
                    if self.levels.is_empty() {
                        self.prev_result(ctx);
                        ctx.render()?;
                    }
                }
                _ => {}
            }
        }
        if let Some(action) = key.claim_directories() {
            match action {
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    // `d` mirrors Right (the footer buttons / a child list).
                    if !self.open_selected_submenu() {
                        if self.active_section_mut().buttons_focused() {
                            self.active_section_mut().move_button_focus(true);
                        } else if !self.active_section_mut().focus_buttons()
                            && self.active_section_mut().confirm(ctx)?
                        {
                            self.destroy(ctx)?;
                        }
                        ctx.render()?;
                    }
                }
                DirectoriesActions::FolderCollapse => {
                    // `a` mirrors Left.
                    if self.active_section_mut().buttons_focused() {
                        self.active_section_mut().move_button_focus(false);
                    } else if self.levels.is_empty() {
                        self.active_section_mut().left();
                    } else {
                        self.close_level();
                    }
                    ctx.render()?;
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        let position: Position = event.into();
        match event.kind {
            MouseEventKind::LeftClick => {
                // Inside the popup the click selects/ticks the row, runs a
                // footer button or opens the row's child list — the same
                // window the keyboard walks. A click *outside* is inert for a
                // keyboard-raised popup (round 88, user): dismissing it on a
                // stray click threw it away, e.g. the click that opened a level
                // landed outside the window the level re-centred to, or a click
                // during the in-place refresh before the popup was re-rendered.
                // A mouse-raised (anchored) popup is the exception — an outside
                // click dismisses it (round 88.1); see the branch below.
                self.drag_scroll = None;
                if let Some(section) = self.levels.last_mut().map(|level| &mut level.sections[0])
                {
                    section.end_check_drag();
                } else {
                    for section in &mut self.sections {
                        section.end_check_drag();
                    }
                }
                if !self.popup_area.contains(position) {
                    // A popup raised at the pointer — a right-click context
                    // menu (`anchor`, round 47) — is dismissed by a click on
                    // the UI behind it; the user asked for that gesture back
                    // for mouse-spawned menus (round 88.1).
                    //
                    // A popup the keyboard opened (centred, or opened from
                    // inside another modal) is NOT dismissed this way: a stray
                    // click must not throw it away. That was the round-88
                    // report — the click that opened a level landing outside
                    // the re-centred window, and a click landing between an
                    // in-place refresh and its next render, when the fresh
                    // popup still had no recorded area (hence `is_empty`: no
                    // geometry means no dismissal).
                    if self.anchor.is_some() && !self.popup_area.is_empty() {
                        return self.destroy(ctx);
                    }
                    return Ok(());
                }
                self.handle_left_click(position, event.modifiers, ctx)?;
            }
            MouseEventKind::DoubleClick => {
                if !self.popup_area.contains(position) {
                    return Ok(());
                }
                // A checkbox row or a row that opens a child list was already
                // handled by the first click of the pair.
                if self.row_has_submenu_at(position) || self.row_is_check_at(position) {
                    ctx.render()?;
                    return Ok(());
                }
                match self.section_at_position_mut(position) {
                    Some(section) => {
                        section.double_click(position, ctx)?;
                    }
                    None => return Ok(()),
                }
                if ctx.input.is_insert_mode() {
                    ctx.render()?;
                } else {
                    self.destroy(ctx)?;
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
            MouseEventKind::Drag { drag_start_position } => {
                // Drag select: every checkbox row between the row the drag
                // started on and the row under the pointer is ticked.
                let Some(from) = self.row_idx_at(drag_start_position) else {
                    return Ok(());
                };
                if let Some(list) = self.active_section().list_area() {
                    let below = position.y > list.bottom().saturating_sub(1);
                    let above = position.y < list.y;
                    if above || below {
                        // Held past the edge: scroll (and tick) at a speed that
                        // grows with the distance from the list. `render` keeps
                        // the scrolling going for as long as the pointer stays
                        // there.
                        let overflow = if below {
                            position
                                .y
                                .saturating_sub(list.bottom().saturating_sub(1))
                        } else {
                            list.y.saturating_sub(position.y)
                        };
                        let rows = (usize::from(overflow) + 1).min(MAX_DRAG_SCROLL_ROWS);
                        let scroll = DragScroll { rows, down: below };
                        self.drag_scroll = Some(scroll);
                        if !self.active_section_mut().drag_scroll(rows, below) {
                            self.drag_scroll = None;
                        }
                        ctx.render()?;
                        return Ok(());
                    }
                }
                self.drag_scroll = None;
                if let Some(to) = self.row_idx_at(position)
                    && self.active_section_mut().drag_select(from, to)
                {
                    ctx.render()?;
                }
            }
            MouseEventKind::LeftRelease => {
                // The drag ended (its painted ticks stay).
                self.drag_scroll = None;
                self.active_section_mut().end_check_drag();
            }
            MouseEventKind::Moved => {
                // The pointer moved with no button held.
                self.drag_scroll = None;
                self.active_section_mut().end_check_drag();
            }
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
            anchor: None,
            popup_area: Rect::default(),
            levels: Vec::new(),
            drag_scroll: None,
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
        for level in &mut self.levels {
            for s in &mut level.sections {
                s.on_close(ctx)?;
            }
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
    /// Anchor the popup at a mouse position (right-click menus): the
    /// content's top-left lands under the cursor, clamped so the popup
    /// never leaves the frame. `None` keeps the centered placement
    /// (keyboard-opened menus and menus opened from inside other modals).
    pub fn anchor(mut self, anchor: Option<Position>) -> Self {
        self.anchor = anchor;
        self
    }
    pub fn build(mut self) -> Self {
        if let Some((i, s)) = self
            .sections
            .iter_mut()
            .enumerate()
            .find_or_first(|(_, s)| s.len() > 0)
        {
            self.current_section_idx = i;
            s.down();
        }
        self.sections_labels = self
            .sections
            .iter()
            .fold(
                Vec::<Vec<String>>::new(),
                |mut acc, s| {
                    acc.push(s.item_labels_iter().map(|l| l.to_lowercase()).collect());
                    acc
                },
            );
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
    fn next(&mut self) {
        // Inside a level the cursor stays in that level and never wraps: at
        // the last row it stays put.
        if !self.levels.is_empty() {
            self.active_section_mut().move_cursor(true);
            return;
        }
        let result = self.sections[self.current_section_idx].down();
        if !result {
            self.current_section_idx = (self.current_section_idx + 1)
                % self.sections.len();
            self.sections[self.current_section_idx].down();
        }
    }
    fn prev(&mut self) {
        if !self.levels.is_empty() {
            self.active_section_mut().move_cursor(false);
            return;
        }
        let result = self.sections[self.current_section_idx].up();
        if !result {
            self.current_section_idx = (self.current_section_idx + self.sections.len()
                - 1) % self.sections.len();
            self.sections[self.current_section_idx].up();
        }
    }
    /// The section that receives input: the popup's current section, or the
    /// deepest open level's only section.
    fn active_section(&self) -> &SectionType<'a> {
        match self.levels.last() {
            Some(level) => &level.sections[0],
            None => &self.sections[self.current_section_idx],
        }
    }

    fn active_section_mut(&mut self) -> &mut SectionType<'a> {
        match self.levels.last_mut() {
            Some(level) => &mut level.sections[0],
            None => &mut self.sections[self.current_section_idx],
        }
    }

    /// The breadcrumb of level `idx`: the labels of the levels leading to it.
    fn level_path(&self, idx: usize) -> String {
        self.levels[..=idx]
            .iter()
            .map(|level| level.label.as_str())
            .collect::<Vec<_>>()
            .join(": ")
    }

    /// Pushes a new level: the popup then shows it instead of its own sections
    /// (its list starts on its first selectable row).
    fn push_level(&mut self, label: String, children: ListSection, parent_row: usize) {
        // The child list needs its content length set before it can select a
        // row (the popup's own sections get the same treatment in
        // `list_section`), otherwise the level opens with no cursor.
        let mut children = children;
        children.state.set_content_len(Some(children.items.len()));
        let mut section = SectionType::Menu(children);
        section.down();
        self.levels.push(SubLevel {
            sections: vec![section],
            areas: vec![Rect::default()],
            label,
            parent_row,
        });
    }

    /// Opens the selected row's child list: the popup shows it (the keyboard's
    /// `Enter`/`Right` and a mouse click both call this). False when the row
    /// has no child list.
    fn open_selected_submenu(&mut self) -> bool {
        let Some(parent_row) = self.active_section().selected() else {
            return false;
        };
        let Some((label, children)) = self.active_section_mut().take_selected_submenu()
        else {
            return false;
        };
        self.push_level(label, children, parent_row);
        true
    }

    /// The label of the selected row on each open level, outermost first: the
    /// popup's own cursor row, then the cursor row of every open submenu
    /// level. A popup refreshed in place is rebuilt from scratch (starting on
    /// its own sections), so this chain is what lets the rebuild put the user
    /// back where they were (`restore_row_path`, round 88).
    pub fn selected_row_path(&self) -> Vec<String> {
        let mut path = Vec::new();
        for section in std::iter::once(&self.sections[self.current_section_idx])
            .chain(self.levels.iter().map(|level| &level.sections[0]))
        {
            if let Some(idx) = section.selected()
                && let Some(label) = section.item_labels_iter().nth(idx)
            {
                path.push(label.to_owned());
            }
        }
        path
    }

    /// Selects the row labelled `label`: in the deepest open level when one is
    /// open, else in the popup's own sections (the section holding the row
    /// becomes the current one). False when no row carries that label.
    fn select_row_by_label(&mut self, label: &str, ctx: &Ctx) -> bool {
        if let Some(level) = self.levels.last_mut() {
            let Some(row) = section_row_by_label(&level.sections[0], label) else {
                return false;
            };
            level.sections[0].select(row);
            return true;
        }
        let Some(section_idx) = self
            .sections
            .iter()
            .position(|section| section_row_by_label(section, label).is_some())
        else {
            return false;
        };
        if section_idx != self.current_section_idx {
            self.sections[self.current_section_idx].unselect(ctx);
            self.current_section_idx = section_idx;
        }
        let Some(row) = section_row_by_label(&self.sections[section_idx], label) else {
            return false;
        };
        self.sections[section_idx].select(row);
        true
    }

    /// Puts the cursor back where `selected_row_path` found it and re-opens
    /// the submenu levels that were open (round 88): every row of the chain
    /// but the last is selected and its child list opened again, the last row
    /// is only selected. A row the refreshed popup no longer has stops the
    /// replay (the rest of the chain is dropped). False when nothing at all
    /// could be restored.
    pub fn restore_row_path(&mut self, path: &[String], ctx: &Ctx) -> bool {
        let Some((last, open)) = path.split_last() else {
            return false;
        };
        for label in open {
            if !self.select_row_by_label(label, ctx) || !self.open_selected_submenu() {
                return false;
            }
        }
        self.select_row_by_label(last, ctx)
    }

    /// Closes the deepest level, handing its list back to the row it was
    /// opened from (so ticked boxes survive a reopen) and restoring that row
    /// as the cursor.
    fn close_level(&mut self) {
        let Some(level) = self.levels.pop() else {
            return;
        };
        let SubLevel {
            mut sections,
            parent_row,
            ..
        } = level;
        if let Some(SectionType::Menu(children)) = sections.pop() {
            let parent = match self.levels.last_mut() {
                Some(parent) => &mut parent.sections[0],
                None => &mut self.sections[self.current_section_idx],
            };
            parent.restore_submenu(parent_row, children);
            parent.select(parent_row);
        }
    }

    /// The section that receives a mouse position: the open level (only it is
    /// drawn), or the popup section under the pointer.
    fn section_at_position_mut(&mut self, position: Position) -> Option<&mut SectionType<'a>> {
        if !self.levels.is_empty() {
            return self.levels.last_mut().map(|level| &mut level.sections[0]);
        }
        let idx = self.section_idx_at_position(position)?;
        self.sections.get_mut(idx)
    }

    /// The row index a position lands on (the open level, or the popup section
    /// under the pointer).
    fn row_idx_at(&self, position: Position) -> Option<usize> {
        match self.levels.last() {
            Some(level) => level.sections[0].row_idx_at_position(position),
            None => self
                .section_idx_at_position(position)
                .and_then(|idx| self.sections[idx].row_idx_at_position(position)),
        }
    }

    /// Whether the row under `position` opens a child list.
    fn row_has_submenu_at(&self, position: Position) -> bool {
        match self.levels.last() {
            Some(level) => level.sections[0].row_has_submenu_at(position),
            None => self
                .section_idx_at_position(position)
                .is_some_and(|idx| self.sections[idx].row_has_submenu_at(position)),
        }
    }

    /// Whether the row under `position` is a checkbox row (the chapter
    /// picker): a click only ticks it.
    fn row_is_check_at(&self, position: Position) -> bool {
        match self.levels.last() {
            Some(level) => level.sections[0].row_is_check_at(position),
            None => self
                .section_idx_at_position(position)
                .is_some_and(|idx| self.sections[idx].row_is_check_at(position)),
        }
    }

    /// A left click inside the popup: runs a footer button, ticks a checkbox
    /// row (with the modifiers), selects a row and opens the clicked row's
    /// child list.
    fn handle_left_click(
        &mut self,
        position: Position,
        modifiers: KeyModifiers,
        ctx: &mut Ctx,
    ) -> Result<()> {
        // A single click on a footer button runs it (Download / Cancel).
        if let Some(idx) = self.active_section_mut().button_at_position(position) {
            if self.active_section_mut().activate_button(idx, ctx)?.is_some() {
                return self.destroy(ctx);
            }
            ctx.render()?;
            return Ok(());
        }
        // shift+click extends the ticked range, ctrl/alt+click ticks the row
        // under the pointer.
        if let Some(row) = self.row_idx_at(position) {
            if modifiers.contains(KeyModifiers::SHIFT) {
                if self.active_section_mut().select_range_to(row) {
                    ctx.render()?;
                }
                return Ok(());
            }
            if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && self.active_section_mut().toggle_check_row(row)
            {
                ctx.render()?;
                return Ok(());
            }
        }
        // A plain click selects the row (and ticks it in a picker).
        let on_row = self.row_idx_at(position).is_some();
        if !on_row {
            return Ok(());
        }
        match self.section_at_position_mut(position) {
            Some(section) => {
                section.left_click(position, ctx);
            }
            None => return Ok(()),
        }
        // A row with a child list opens it (the mouse walks the same tree as
        // the keyboard).
        self.open_selected_submenu();
        ctx.render()?;
        Ok(())
    }

    fn section_idx_at_position(&self, position: Position) -> Option<usize> {
        self.areas.iter().enumerate().find(|(_, a)| a.contains(position)).map(|(i, _)| i)
    }
}

/// The index of the first row labelled `label` in `section`
/// (case-insensitive; `item_labels_iter` is indexed like the section's rows).
fn section_row_by_label(section: &SectionType<'_>, label: &str) -> Option<usize> {
    section.item_labels_iter().position(|row| row.eq_ignore_ascii_case(label))
}

/// The fastest drag auto-scroll: rows per frame when the pointer is held far
/// past the list's edge.
const MAX_DRAG_SCROLL_ROWS: usize = 8;

/// Height of one menu window holding `sections`: its rows plus the separator
/// rows and the two border rows. The popup and every submenu level use the
/// same formula.
fn menu_window_height(sections: &[SectionType<'_>]) -> u16 {
    let content: usize = sections
        .iter()
        .map(|section| section.preferred_height() as usize)
        .sum::<usize>() + 1 + sections.len();
    content as u16
}

/// Draws one menu window (its border, title, sections and the separators
/// between them) and records each section's area. Shared by the popup itself
/// and every open submenu level.
fn draw_menu_window(
    frame: &mut Frame,
    ctx: &Ctx,
    sections: &mut [SectionType<'_>],
    areas: &mut [Rect],
    window: Rect,
    title: Option<&str>,
    filter: Option<&str>,
) {
    frame.render_widget(Clear, window);
    if let Some(bg_color) = ctx.config.theme.modal_background_color {
        frame.render_widget(
            Block::default().style(Style::default().bg(bg_color)),
            window,
        );
    }
    let title = match filter {
        Some(filter) => format!(" {FILTER_PREFIX}: {filter} "),
        None => title.unwrap_or_default().to_owned(),
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(ctx.config.as_border_style())
        .title_alignment(ratatui::prelude::Alignment::Center);
    if !title.is_empty() {
        block = block.title(title);
    }
    let content_area = block.inner(window);
    let split = Layout::vertical(
        Itertools::intersperse(
            sections
                .iter_mut()
                .map(|s| Constraint::Length(s.preferred_height())),
            Constraint::Length(1),
        ),
    )
    .split(content_area);
    let mut section_idx = 0;
    for (idx, area) in split.iter().enumerate() {
        if idx % 2 == 0 {
            sections[section_idx].render(*area, frame.buffer_mut(), filter, ctx);
            areas[section_idx] = *area;
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
    frame.render_widget(block, window);
}

/// tfm-style clamped popup rect at a mouse position: the menu's top-left
/// lands under the cursor, shifted back so its right/bottom edges never
/// leave the frame (`px = max(0, frame_width - w - 1)`, same for y). A
/// menu taller than the terminal pins to the top and overflows over the
/// bottom edge, exactly like the centered placement does.
fn anchor_rect(anchor: Position, width: u16, height: u16, frame: Rect) -> Rect {
    let mut x = anchor.x;
    let mut y = anchor.y;
    if x.saturating_add(width).saturating_add(1) > frame.width {
        x = frame.width.saturating_sub(width).saturating_sub(1);
    }
    if y.saturating_add(height).saturating_add(1) > frame.height {
        y = frame.height.saturating_sub(height).saturating_sub(1);
    }
    Rect { x, y, width, height }
}


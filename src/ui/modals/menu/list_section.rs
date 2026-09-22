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

/// One row in a `ListSection`. Two kinds:
///
/// - **Action items** (`add_item`): running the row's closure on confirm.
/// - **Select items** (`add_select_item`): carrying a value that the
///   section-level `action` callback receives on confirm (the old
///   `SelectSection` shape — merged here so menu and value-picker
///   sections are one implementation).
///
/// Either kind can be a `disabled` header row (e.g. "[Audio]" group
/// labels): rendered dim, skipped by navigation, never confirmed.
#[derive(derive_more::Debug)]
pub struct MenuItem {
    pub label: String,
    /// `Some` for select items: the value handed to the section's
    /// `on_select` callback when this row is confirmed.
    pub value: Option<String>,
    #[debug(skip)]
    pub on_confirm: Option<Box<dyn FnOnce(&Ctx) -> Result<()> + Send + Sync + 'static>>,
    /// Header rows (e.g. "[Audio]" group labels): rendered dim, skipped by
    /// navigation and never confirmed.
    pub disabled: bool,
    /// A child list opened from this row (round 78): the row draws a `>`
    /// marker and has no action of its own. The keyboard path moves the
    /// children into the popup in place, the mouse path draws them in a
    /// flyout box beside the row.
    pub submenu: Option<Box<ListSection>>,
    /// `Some` for checkbox rows (the download chapter picker): rendered as
    /// `⭘`/`●`, toggled by `Enter` on the row, `CommonAction::Select` and a
    /// left click.
    pub checked: Option<bool>,
}

#[derive(derive_more::Debug, Default)]
pub struct ListSection {
    pub items: Vec<MenuItem>,
    pub areas: EnumMap<ListSectionArea, Rect>,
    pub current_item_style: Style,
    max_height: Option<usize>,
    /// Checkbox-list confirm callback: receives the ticked row indices in
    /// list order and the index of the activate confirm button (0 for the
    /// plain `Download`; the multi-choice picker's `Audio`=0 / `Video`=1).
    #[debug(skip)]
    check_confirm: Option<
        Box<dyn FnOnce(&Ctx, Vec<usize>, usize) -> Result<()> + Send + Sync + 'static>,
    >,
    /// Footer buttons of a checkbox list (see `add_check_buttons`).
    pub buttons: Vec<ListButton>,
    /// `Some(idx)` while the footer buttons hold the focus (the list rows are
    /// then rendered unfocused).
    button_focus: Option<usize>,
    /// The row a range selection (shift-click, drag, Shift+Up/Down) extends
    /// from; kept up to date by plain navigation and clicks.
    anchor: Option<usize>,
    /// The running drag toggle (`None` = no drag).
    check_drag: Option<CheckDrag>,
    pub state: DirState<ListState>,
    /// Select-section callback: receives the confirmed item's value
    /// (`add_select_item` rows). Mutually exclusive with per-item
    /// `on_confirm` closures; a section uses one or the other.
    #[debug(skip)]
    on_select: Option<Box<dyn FnOnce(&Ctx, String) -> Result<()> + Send + Sync + 'static>>,
    /// Runs once when the modal this section belongs to closes (e.g. the
    /// paste popup clears its scan state when it is dismissed).
    #[debug(skip)]
    on_close: Option<Box<dyn FnOnce(&Ctx) + Send + Sync + 'static>>,
    /// `Enter` on a row activates the first footer button instead of moving
    /// the focus onto it (see `set_confirm_on_enter`).
    enter_confirms: bool,
}

#[derive(Copy, Clone, Debug, Enum, Eq, PartialEq, Hash)]
pub enum ListSectionArea {
    List = 0,
    Scrollbar = 1,
    /// The footer button row of a checkbox list (`Download` / `Cancel`).
    Buttons = 2,
}

/// A footer button of a checkbox list (round 78 follow-up): rendered under the
/// list, reached with `Enter` from a row or with a click, moved between with
/// `Left`/`Right`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListButtonKind {
    /// Runs the checkbox list's confirm callback with the ticked rows and this
    /// button's index (0 for the plain `Download`, the multi-choice picker's
    /// `Audio`=0 / `Video`=1).
    Confirm(usize),
    /// Closes the picker's modal.
    Cancel,
}

/// A running drag toggle in a checkbox list: the flip the drag paints (chosen
/// from the anchor row when the drag started) and the rows it has touched, so
/// a row that leaves the drag's range goes back to the state it had before the
/// drag — items ticked by `Space`/ctrl+click outside the range are never
/// touched.
#[derive(Debug, Default)]
struct CheckDrag {
    /// Whether a drag is running (the `Default` state is "no drag").
    running: bool,
    /// `true` = the drag ticks the rows it is dragged over (the anchor row was
    /// unticked), `false` = it unticks them.
    paint: bool,
    /// The row the press landed on.
    anchor: usize,
    /// `(row, state before the drag)` for every row the drag has painted.
    touched: Vec<(usize, bool)>,
}

#[derive(derive_more::Debug)]
pub struct ListButton {
    pub label: String,
    pub kind: ListButtonKind,
    /// Dim and inert while `true` (nothing ticked yet).
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
            check_confirm: None,
            buttons: Vec::new(),
            button_focus: None,
            anchor: None,
            check_drag: None,
            state: DirState::default(),
            on_select: None,
            on_close: None,
            enter_confirms: false,
        }
    }

    pub fn item(
        mut self,
        label: impl Into<String>,
        on_confirm: impl FnOnce(&Ctx) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.items.push(MenuItem {
            label: label.into(),
            value: None,
            on_confirm: Some(Box::new(on_confirm)),
            disabled: false,
            submenu: None,
            checked: None,
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
            value: None,
            on_confirm: Some(Box::new(on_confirm)),
            disabled: false,
            submenu: None,
            checked: None,
        });
        self
    }

    /// A value-picker row (the old `SelectSection` shape): the section's
    /// `action` callback receives `value` when this row is confirmed.
    pub fn add_select_item(
        &mut self,
        label: impl Into<String>,
        value: impl Into<String>,
    ) -> &mut Self {
        self.items.push(MenuItem {
            label: label.into(),
            value: Some(value.into()),
            on_confirm: None,
            disabled: false,
            submenu: None,
            checked: None,
        });
        self
    }

    /// The select-section confirm callback, receiving the confirmed row's
    /// value. Only meaningful together with `add_select_item` rows.
    pub fn action(
        &mut self,
        on_select: impl FnOnce(&Ctx, String) -> Result<()> + Send + Sync + 'static,
    ) -> &mut Self {
        self.on_select = Some(Box::new(on_select));
        self
    }

    /// A dim, non-selectable header row (e.g. "[Audio]" / "[Video]" group
    /// labels inside one list).
    pub fn header(&mut self, label: impl Into<String>) -> &mut Self {
        self.items.push(MenuItem {
            label: label.into(),
            value: None,
            on_confirm: None,
            disabled: true,
            submenu: None,
            checked: None,
        });
        self
    }

    /// A row that opens a child list (round 78): drawn with a `>` marker and
    /// carrying no action of its own. `Enter`/`Right` on it (keyboard) moves
    /// the child list into the popup in place; a click (mouse) opens it as a
    /// flyout box beside the row.
    pub fn add_submenu_item(
        &mut self,
        label: impl Into<String>,
        children: ListSection,
    ) -> &mut Self {
        self.items.push(MenuItem {
            label: label.into(),
            value: None,
            on_confirm: None,
            disabled: false,
            submenu: Some(Box::new(children)),
            checked: None,
        });
        self
    }

    /// A checkbox row (the download chapter picker): rendered as `⭘`/`●`.
    pub fn add_check_item(&mut self, label: impl Into<String>, checked: bool) -> &mut Self {
        self.items.push(MenuItem {
            label: label.into(),
            value: None,
            on_confirm: None,
            disabled: false,
            submenu: None,
            checked: Some(checked),
        });
        self
    }

    /// Turns the section into a checkbox list (`add_check_item` +
    /// `add_check_buttons`/`add_choice_buttons`): `on_confirm` receives the
    /// ticked row indices in list order and the index of the confirm button
    /// that was activated.
    pub fn check_list(
        &mut self,
        on_confirm: impl FnOnce(&Ctx, Vec<usize>, usize) -> Result<()> + Send + Sync + 'static,
    ) -> &mut Self {
        self.check_confirm = Some(Box::new(on_confirm));
        self
    }

    /// The ticked rows of a checkbox list, in list order.
    pub fn checked_indices(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.checked == Some(true))
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Flips the checkbox of row `idx` (checkbox rows only) and refreshes the
    /// confirm row's label/state.
    pub fn toggle_row(&mut self, idx: usize) -> bool {
        let Some(checked) = self.items.get(idx).and_then(|item| item.checked) else {
            return false;
        };
        self.items[idx].checked = Some(!checked);
        // The toggled row is where a later range selection starts from.
        self.anchor = Some(idx);
        self.refresh_buttons();
        true
    }

    /// Flips the selected row's checkbox (`CommonAction::Select` / `Enter`).
    pub fn toggle_selected(&mut self) -> bool {
        self.state.get_selected().is_some_and(|idx| self.toggle_row(idx))
    }

    /// The item index under `position` (rendered rows only).
    pub fn item_idx_at_position(&self, position: Position) -> Option<usize> {
        let list_area = self.areas[ListSectionArea::List];
        if !list_area.contains(position) {
            return None;
        }
        let clicked_row: usize = position.y.saturating_sub(list_area.y).into();
        self.state.get_at_rendered_row(clicked_row)
    }

    /// The screen rect of item `idx`'s rendered row.
    pub fn item_area(&self, idx: usize) -> Option<Rect> {
        let list_area = self.areas[ListSectionArea::List];
        let row = idx.checked_sub(self.state.offset())?;
        if row >= list_area.height as usize {
            return None;
        }
        let mut area = list_area.shrink_from_top(row as u16);
        area.height = 1;
        Some(area)
    }

    /// Takes the selected row's child list (rows with a submenu), leaving the
    /// row without one — the modal hands it back when the level closes.
    pub fn take_selected_submenu(&mut self) -> Option<(String, ListSection)> {
        let idx = self.state.get_selected()?;
        let item = self.items.get_mut(idx)?;
        let children = item.submenu.take()?;
        Some((item.label.clone(), *children))
    }

    /// Puts a child list back on row `idx` (the modal returns it when the
    /// level it was opened from closes, so ticked boxes survive a reopen).
    pub fn restore_submenu(&mut self, idx: usize, children: ListSection) {
        if let Some(item) = self.items.get_mut(idx) {
            item.submenu = Some(Box::new(children));
        }
    }

    /// Adds the footer buttons of a checkbox list (the picker's
    /// `Download` / `Cancel`): `Download` runs the `check_list` callback with
    /// the ticked rows (dim while nothing is ticked), `Cancel` closes the
    /// picker's modal. The dim state follows the rows that are ticked at this
    /// point, so a picker built with pre-ticked rows (the queue's multi-stream
    /// download) opens with live buttons.
    pub fn add_check_buttons(
        &mut self,
        download: impl Into<String>,
        cancel: impl Into<String>,
    ) -> &mut Self {
        self.buttons = vec![
            ListButton {
                label: download.into(),
                kind: ListButtonKind::Confirm(0),
                disabled: true,
            },
            ListButton {
                label: cancel.into(),
                kind: ListButtonKind::Cancel,
                disabled: false,
            },
        ];
        self.button_focus = None;
        self.refresh_buttons();
        self
    }

    /// The footer buttons of a multi-choice checkbox list (the queue's
    /// multi-stream download picker: `Audio` / `Video` / `Cancel`). Every
    /// choice button runs the `check_list` callback with the ticked rows and
    /// its own index among the choices, so one picker serves several output
    /// kinds; each is dim while nothing is ticked.
    pub fn add_choice_buttons(
        &mut self,
        choices: &[&str],
        cancel: impl Into<String>,
    ) -> &mut Self {
        self.buttons = choices
            .iter()
            .enumerate()
            .map(|(idx, label)| ListButton {
                label: (*label).to_owned(),
                kind: ListButtonKind::Confirm(idx),
                disabled: true,
            })
            .collect();
        self.buttons.push(ListButton {
            label: cancel.into(),
            kind: ListButtonKind::Cancel,
            disabled: false,
        });
        self.button_focus = None;
        self.refresh_buttons();
        self
    }

    /// True for a checkbox list (`add_check_item` rows).
    pub fn is_check_list(&self) -> bool {
        self.items.iter().any(|item| item.checked.is_some())
    }

    /// Makes `Enter` on a row activate the first footer button directly
    /// (instead of the usual move-the-focus-onto-it step). The multi-stream
    /// download picker uses it: every row is ticked already, so one `Enter`
    /// on the list starts the downloads. The cursor and `Space` behave as in
    /// any checkbox list.
    pub fn set_confirm_on_enter(&mut self) -> &mut Self {
        self.enter_confirms = true;
        self
    }

    /// `Enter` on a row activates the first footer button directly (see
    /// `set_confirm_on_enter`): true when the activation happened and the
    /// modal should close. False for every other section and while nothing is
    /// ticked (the confirm buttons are inert then, so `Enter` moves the focus
    /// onto them the usual way).
    pub fn confirm_on_enter(&mut self, ctx: &Ctx) -> Result<bool> {
        if !self.enter_confirms || self.checked_indices().is_empty() {
            return Ok(false);
        }
        Ok(self.activate_button(0, ctx)?.is_some())
    }

    /// True while the footer buttons hold the focus.
    pub fn buttons_focused(&self) -> bool {
        self.button_focus.is_some()
    }

    /// Moves the focus onto the first footer button (`Enter` from a row).
    /// False when the section has no buttons.
    pub fn focus_buttons(&mut self) -> bool {
        if self.buttons.is_empty() {
            return false;
        }
        self.button_focus = Some(0);
        self.refresh_buttons();
        true
    }

    pub fn clear_button_focus(&mut self) {
        self.button_focus = None;
    }

    /// Moves the button focus (`Left`/`Right`): false when it walked off the
    /// button row (back to the list).
    pub fn move_button_focus(&mut self, forward: bool) -> bool {
        let Some(idx) = self.button_focus else {
            return false;
        };
        if forward {
            if idx + 1 < self.buttons.len() {
                self.button_focus = Some(idx + 1);
                return true;
            }
            return false;
        }
        if idx > 0 {
            self.button_focus = Some(idx - 1);
            return true;
        }
        self.button_focus = None;
        false
    }

    /// The footer button under `position`.
    pub fn button_at_position(&self, position: Position) -> Option<usize> {
        if self.areas[ListSectionArea::Buttons].width == 0 {
            return None;
        }
        self.button_areas().into_iter().position(|area| {
            position.y == area.y && position.x >= area.x && position.x < area.right()
        })
    }

    /// The screen rects of the footer buttons (mirrors `render_buttons`).
    fn button_areas(&self) -> Vec<Rect> {
        let area = self.areas[ListSectionArea::Buttons];
        if area.width == 0 {
            return Vec::new();
        }
        let mut areas = Vec::new();
        let mut x = area.x;
        for button in &self.buttons {
            let width = button.label.chars().count() as u16 + 4;
            if x.saturating_add(width) > area.right() {
                break;
            }
            areas.push(Rect {
                x,
                y: area.y,
                width,
                height: 1,
            });
            x = x.saturating_add(width).saturating_add(1);
        }
        areas
    }

    /// Activates footer button `idx`: `Some(true)` = the modal should close.
    pub fn activate_button(&mut self, idx: usize, ctx: &Ctx) -> Result<Option<bool>> {
        let Some((kind, disabled)) = self
            .buttons
            .get(idx)
            .map(|button| (button.kind, button.disabled))
        else {
            return Ok(None);
        };
        if disabled {
            return Ok(None);
        }
        match kind {
            ListButtonKind::Confirm(choice) => {
                let checked = self.checked_indices();
                if checked.is_empty() {
                    return Ok(None);
                }
                if let Some(cb) = self.check_confirm.take() {
                    (cb)(ctx, checked, choice)?;
                }
                Ok(Some(true))
            }
            ListButtonKind::Cancel => Ok(Some(true)),
        }
    }

    /// Activates the focused footer button (`Enter`).
    pub fn activate_focused_button(&mut self, ctx: &Ctx) -> Result<Option<bool>> {
        match self.button_focus {
            Some(idx) => self.activate_button(idx, ctx),
            None => Ok(None),
        }
    }

    /// Ticks every checkbox row between the anchor and `idx` (drag select and
    /// shift-click). False when the section has no checkbox rows.
    pub fn select_range_to(&mut self, idx: usize) -> bool {
        if !self.is_check_list() || idx >= self.items.len() {
            return false;
        }
        let anchor = self.anchor.unwrap_or(idx).min(self.items.len() - 1);
        let (from, to) = if anchor <= idx { (anchor, idx) } else { (idx, anchor) };
        for row in from..=to {
            if self.items[row].checked.is_some() {
                self.items[row].checked = Some(true);
            }
        }
        self.state.select(Some(idx), 0);
        self.anchor = Some(anchor);
        self.refresh_buttons();
        true
    }

    /// The list's screen rect (the rows, without the footer buttons).
    pub fn list_area(&self) -> Option<Rect> {
        let area = self.areas[ListSectionArea::List];
        (area.width > 0 && area.height > 0).then_some(area)
    }

    /// Drag auto-scroll: moves the cursor `rows` rows towards `down` (ticking
    /// every checkbox row the drag passed) and scrolls the list along with it.
    /// False when the list is already at that end.
    pub fn drag_scroll(&mut self, rows: usize, down: bool) -> bool {
        let Some(current) = self.state.get_selected() else {
            return false;
        };
        let last = self.items.len().saturating_sub(1);
        let target = if down {
            current.saturating_add(rows).min(last)
        } else {
            current.saturating_sub(rows)
        };
        if target == current {
            return false;
        }
        // The auto-scroll paints with the same drag toggle (a drag started on a
        // ticked row keeps un-ticking as it scrolls).
        let from = self
            .check_drag
            .as_ref()
            .map_or(self.anchor.unwrap_or(current), |drag| drag.anchor);
        self.apply_check_drag(from, target)
    }

    /// Moves the cursor one row **without wrapping**: at the first/last
    /// selectable row it stays there (a menu level has no next section to hand
    /// the movement to, so the `down`/`up` end-of-list unselect would make the
    /// cursor jump back to the other end).
    pub fn move_cursor(&mut self, down: bool) -> bool {
        if self.button_focus.is_some() {
            // Down from the footer buttons returns to the list.
            self.button_focus = None;
            return true;
        }
        let Some(current) = self.state.get_selected() else {
            self.repair_cursor();
            return true;
        };
        let target = if down {
            (current.saturating_add(1)..self.items.len())
                .find(|idx| !self.items[*idx].disabled)
        } else {
            (0..current).rev().find(|idx| !self.items[*idx].disabled)
        };
        match target {
            Some(idx) => {
                self.select(idx);
                true
            }
            None => false,
        }
    }

    /// Puts the cursor back on the nearest selectable row (a level whose
    /// selection was lost at an end keeps its cursor instead of jumping to the
    /// other end).
    fn repair_cursor(&mut self) {
        let Some(idx) = self.items.iter().position(|item| !item.disabled) else {
            return;
        };
        self.state.select(Some(idx), 0);
        self.anchor = Some(idx);
    }

    /// Drag toggle: paints every checkbox row between the drag's start row and
    /// the row under the pointer with the flip chosen when the drag started (a
    /// ticked anchor row unticks what the drag is dragged over, an unticked one
    /// ticks it). A row that leaves the range again goes back to its state from
    /// before the drag, so `Space`/ctrl+click ticks outside the dragged range
    /// stay untouched.
    pub fn drag_select(&mut self, from: usize, to: usize) -> bool {
        self.apply_check_drag(from, to)
    }

    /// Ends a running drag toggle (the button was released, the pointer came
    /// back inside or a key was pressed): the painted state stays.
    pub fn end_check_drag(&mut self) {
        self.check_drag = None;
    }

    /// The drag toggle's core: `from` is the row the drag started on (its flip
    /// is decided there) and `to` the row currently under the pointer (or the
    /// row the auto-scroll moved onto).
    fn apply_check_drag(&mut self, from: usize, to: usize) -> bool {
        if !self.is_check_list() || from >= self.items.len() || to >= self.items.len() {
            return false;
        }
        let mut drag = self.check_drag.take().unwrap_or_default();
        // A new drag (or one that started on another row) takes its flip from
        // the anchor row.
        if !drag.running {
            // No press started this drag (a programmatic one): take the flip
            // from the anchor row as it is now.
            let Some(checked) = self.items[from].checked else {
                return false;
            };
            drag = CheckDrag {
                running: true,
                paint: !checked,
                anchor: from,
                touched: Vec::new(),
            };
        }
        let (low, high) = if from <= to { (from, to) } else { (to, from) };
        // Rows the drag left again go back to their pre-drag state.
        let mut idx = 0;
        while idx < drag.touched.len() {
            let (row, before) = drag.touched[idx];
            if row < low || row > high {
                if self.items[row].checked.is_some() {
                    self.items[row].checked = Some(before);
                }
                drag.touched.swap_remove(idx);
            } else {
                idx += 1;
            }
        }
        // Paint the rows in the current range.
        for row in low..=high {
            if self.items[row].checked.is_none() {
                continue;
            }
            if !drag.touched.iter().any(|(touched, _)| *touched == row) {
                drag.touched
                    .push((row, self.items[row].checked.unwrap_or(false)));
            }
            self.items[row].checked = Some(drag.paint);
        }
        self.check_drag = Some(drag);
        self.state.select(Some(to), 0);
        self.anchor = Some(from);
        self.refresh_buttons();
        true
    }

    /// Extends the ticked range by one row (`Shift+Down` / `Shift+Up`).
    pub fn extend_check_selection(&mut self, down: bool) -> bool {
        let Some(current) = self.state.get_selected() else {
            return false;
        };
        let next = if down {
            current.saturating_add(1)
        } else {
            current.saturating_sub(1)
        };
        if next == current || next >= self.items.len() {
            return false;
        }
        self.select_range_to(next)
    }

    /// Keeps the footer buttons in step with the ticked rows (the `Download`
    /// button is inert while nothing is ticked).
    fn refresh_buttons(&mut self) {
        if self.buttons.is_empty() {
            return;
        }
        let any = !self.checked_indices().is_empty();
        for button in &mut self.buttons {
            if matches!(button.kind, ListButtonKind::Confirm(_)) {
                button.disabled = !any;
            }
        }
    }

    /// Draws the footer buttons (`[ Download ]  [ Cancel ]`), the focused one
    /// highlighted and a disabled one dim.
    fn render_buttons(&self, buf: &mut Buffer, ctx: &Ctx) {
        let text_style = ctx
            .config
            .theme
            .text_color
            .map_or_else(Style::default, |color| Style::default().fg(color));
        for (idx, button_area) in self.button_areas().into_iter().enumerate() {
            let button = &self.buttons[idx];
            let style = if button.disabled {
                text_style.add_modifier(ratatui::style::Modifier::DIM)
            } else if self.button_focus == Some(idx) {
                self.current_item_style
            } else {
                text_style
            };
            Text::raw(format!("[ {} ]", button.label))
                .style(style)
                .render(button_area, buf);
        }
    }

    /// Caps the section's window to `window_height` rows (the picker level uses
    /// 2/3 of the terminal): the list scrolls instead of growing past it.
    pub fn cap_window_height(&mut self, window_height: u16) {
        let chrome = 2 + u16::from(!self.buttons.is_empty());
        self.max_height = Some(window_height.saturating_sub(chrome).max(1) as usize);
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
        // The footer buttons are below the list: Down from them goes back to
        // the list instead of wrapping.
        if self.button_focus.is_some() {
            self.button_focus = None;
            return true;
        }
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
        self.anchor = self.state.get_selected();
        true
    }

    fn up(&mut self) -> bool {
        if self.button_focus.is_some() {
            self.button_focus = None;
            return true;
        }
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
        self.anchor = self.state.get_selected();
        true
    }

    fn selected(&self) -> Option<usize> {
        self.state.get_selected()
    }

    fn select(&mut self, idx: usize) {
        self.state.select(Some(idx), 0);
        self.anchor = Some(idx);
    }

    fn unselect(&mut self, _ctx: &Ctx) {
        let offset = self.state.offset();
        self.state.inner.select(None);
        self.state.set_offset(offset);
    }

    fn confirm(&mut self, ctx: &Ctx) -> Result<bool> {
        let Some(selected_idx) = self.state.get_selected() else {
            return Ok(false);
        };
        if self.items[selected_idx].disabled {
            return Ok(false);
        }
        // A checkbox list is driven by `Space`/clicks (ticks) and its footer
        // buttons: `Enter` on a row only moves the focus to them, which the
        // modal does before it calls this.
        if self.is_check_list() {
            return Ok(false);
        }
        let item = &mut self.items[selected_idx];
        if let Some(cb) = item.on_confirm.take() {
            (cb)(ctx)?;
            return Ok(true);
        }
        if let Some(value) = item.value.take()
            && let Some(cb) = self.on_select.take()
        {
            (cb)(ctx, value)?;
        }
        Ok(true)
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    fn preferred_height(&self) -> u16 {
        let len = self.items.len();
        let rows = self.max_height.map_or(len, |mh| len.min(mh)) as u16;
        rows + u16::from(!self.buttons.is_empty())
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, filter: Option<&str>, ctx: &Ctx) {
        // A checkbox list keeps its footer buttons on the last row and scrolls
        // the rows above them.
        let [area, buttons_area] = if self.buttons.is_empty() {
            [area, Rect::default()]
        } else {
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area)
        };
        self.areas[ListSectionArea::Buttons] = buttons_area;
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
            // A checkbox row draws its box glyph in front of the label.
            let label = match item.checked {
                Some(true) => format!("● {}", item.label),
                Some(false) => format!("⭘ {}", item.label),
                None => item.label.clone(),
            };
            let mut text = Text::raw(label);
            let selected = self.state.get_selected().is_some_and(|i| i == idx)
                && self.button_focus.is_none();

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
            // A row with a child list carries a `>` marker in its right cell
            // (the flyout/submenu affordance of round 78).
            if item.submenu.is_some() && item_area.width > 1 {
                let style = if selected {
                    self.current_item_style
                } else {
                    ctx.config
                        .theme
                        .text_color
                        .map_or_else(Style::default, |c| Style::default().fg(c))
                };
                buf[(item_area.right() - 1, item_area.y)].set_symbol(">").set_style(style);
            }
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
        if !self.buttons.is_empty() {
            self.render_buttons(buf, ctx);
        }
    }

    fn left_click(&mut self, position: Position, _ctx: &Ctx) {
        if let Some(idx) = self.button_at_position(position) {
            // The modal activates footer buttons (they need `ctx`); just focus
            // the clicked one here.
            self.button_focus = Some(idx);
            return;
        }
        self.button_focus = None;
        let clicked = self.item_idx_at_position(position);
        if let Some(idx) = clicked {
            self.anchor = Some(idx);
        }
        self.select_item_at_position(position);
        let Some(idx) = clicked else {
            return;
        };
        if self.items[idx].checked.is_some() {
            // Pressing a checkbox row ticks it *and* starts a drag toggle: the
            // flip the drag will paint is decided from the state before the
            // press (a ticked row unticks what the drag passes next).
            let before = self.items[idx].checked.unwrap_or(false);
            self.check_drag = Some(CheckDrag {
                running: true,
                paint: !before,
                anchor: idx,
                touched: vec![(idx, before)],
            });
            self.items[idx].checked = Some(!before);
            self.refresh_buttons();
        } else {
            // A plain action row: range selection (shift-click, drag) is driven
            // by the modal.
            self.toggle_row(idx);
        }
    }

    fn double_click(&mut self, _pos: Position, ctx: &Ctx) -> Result<bool> {
        self.confirm(ctx)?;
        Ok(false)
    }

    fn item_labels_iter(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        Box::new(self.items.iter().map(|i| i.label.as_str()))
    }
}

use ratatui::{
    buffer::Buffer, layout::Rect, style::Style,
    widgets::{List, ListItem, ListState, StatefulWidget},
};
/// A ratatui `List` that renders from its state's offset, showing only
/// the visible slice. The selection is shifted by the offset so the
/// highlight only appears when the selected row is inside the window —
/// the selection can leave the visible area (round-32 wheel behavior:
/// the wheel scrolls the viewport, not the selection).
#[derive(Debug)]
pub struct VirtualizedList<'a> {
    items: Vec<ListItem<'a>>,
    highlight_style: Style,
    style: Style,
    /// Rows per item (1 for single-line lists, 2 for radio stations).
    row_height: u16,
}
impl<'a> VirtualizedList<'a> {
    pub fn new(items: Vec<ListItem<'a>>) -> Self {
        Self {
            items,
            highlight_style: Style::default(),
            style: Style::default(),
            row_height: 1,
        }
    }
    pub fn highlight_style(mut self, style: Style) -> Self {
        self.highlight_style = style;
        self
    }
    pub fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
    pub fn row_height(mut self, height: u16) -> Self {
        self.row_height = height.max(1);
        self
    }
}
impl<'a> StatefulWidget for VirtualizedList<'a> {
    type State = ListState;
    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let viewport = (area.height as usize) / (self.row_height as usize);
        let original_offset = state
            .offset()
            .min(self.items.len().saturating_sub(viewport));
        let original_selected = state.selected();
        let visible_selected = original_selected
            .and_then(|s| s.checked_sub(original_offset))
            .filter(|s| *s < viewport);
        let visible: Vec<ListItem> = self
            .items
            .into_iter()
            .skip(original_offset)
            .take(viewport)
            .collect();
        let mut render_state = ListState::default();
        render_state.select(visible_selected);
        StatefulWidget::render(
            List::new(visible).highlight_style(self.highlight_style).style(self.style),
            area,
            buf,
            &mut render_state,
        );
        *state.offset_mut() = original_offset;
        state.select(original_selected);
    }
}
/// Scroll a `ListState`'s viewport by `dir * amount` rows without moving
/// the selection (the selection may leave the visible area). The offset
/// clamps at the list's ends.
pub fn scroll_viewport(
    state: &mut ListState,
    dir: i64,
    amount: usize,
    content_len: usize,
    viewport_len: usize,
) {
    let max_offset = content_len.saturating_sub(viewport_len);
    let new_offset = if dir < 0 {
        state.offset().saturating_sub(amount)
    } else {
        state.offset().saturating_add(amount).min(max_offset)
    };
    if new_offset != state.offset() {
        *state.offset_mut() = new_offset;
    }
}
/// Scroll a `ListState` so its selection is visible again after a
/// viewport-only wheel scroll (keyboard moves scroll the selection back
/// into view with the standard scrolloff behavior).
pub fn scroll_selection_into_view(
    state: &mut ListState,
    content_len: usize,
    viewport_len: usize,
    scrolloff: usize,
) {
    let Some(selected) = state.selected() else { return };
    if viewport_len == 0 {
        return;
    }
    let max_offset = content_len.saturating_sub(viewport_len);
    let offset = state.offset();
    if scrolloff.saturating_mul(2) >= viewport_len {
        *state.offset_mut() = selected.saturating_sub(viewport_len / 2).min(max_offset);
        return;
    }
    let scrolloff_start_down = (offset.saturating_add(viewport_len))
        .saturating_sub(scrolloff.saturating_add(1));
    if selected > scrolloff_start_down {
        *state.offset_mut() = (offset
            .saturating_add(selected.saturating_sub(scrolloff_start_down)))
            .min(max_offset);
        return;
    }
    if selected < offset.saturating_add(scrolloff) {
        *state.offset_mut() = offset
            .saturating_sub((offset.saturating_add(scrolloff)).saturating_sub(selected));
    }
}
